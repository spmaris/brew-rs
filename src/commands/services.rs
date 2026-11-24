//! Read-only service previews for brewrs-managed packages.
//!
//! This module has no start, stop, registration, supervision, filesystem-write,
//! launchd-query, process-query, or service-state operation. It renders the
//! reviewed Colima 0.10.3 trial contract from local receipts and, for other
//! installed formulae, an official `service` declaration that has been pinned
//! to the exact installed receipt. [`list`] only reports what it can observe in
//! two brewrs-owned state roots; it never claims that a service is running.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{config::Config, ui::DnfStyleFormatter};

use super::install::prefix_path;
use crate::domain::plan::{self, InstalledServiceMetadata};
use crate::infra::formula_source::fetch_installed_service;
use crate::infra::platform::default_target;
use crate::infra::store::{Receipt, Store, validate_dependencies_with_retained};

// Flags were checked against the Colima 0.10.3 start command. The preview is
// deliberately separate from the upstream daemon definition: it uses a new
// state directory, does not activate a container context or set up SSH config,
// and does not mount the user's home directory.
const COLIMA_ARGS: &[&str] = &[
    "start",
    "--foreground",
    "--activate=false",
    "--ssh-config=false",
    "--mount=none",
    "--template=false",
];
const COLIMA_VERSION: &str = "0.10.3";

/// Future isolated lifecycle state root. Nothing in this module creates it.
const SERVICE_STATE_ROOT: &str = ".brewrs/services";
/// Existing reviewed preview root for the Colima trial. The preview itself
/// refuses to reuse a non-fresh trial path.
const SERVICE_TRIAL_ROOT: &str = ".brewrs/service-trials";

/// Maximum argv elements accepted from one `service.run` declaration.
const MAX_SERVICE_ARGS: usize = 64;
/// Maximum length of one argv element, path, or environment value.
const MAX_SERVICE_LITERAL_LEN: usize = 4096;
/// Maximum declared environment variables.
const MAX_SERVICE_ENV: usize = 64;
/// Maximum length of an environment variable name.
const MAX_SERVICE_ENV_NAME_LEN: usize = 64;
/// Maximum length of a service label or declared service name.
const MAX_SERVICE_NAME_LEN: usize = 255;
/// Maximum length of a rendered `run_type`.
const MAX_RUN_TYPE_LEN: usize = 64;
/// Maximum length of a declared service key.
const MAX_SERVICE_KEY_LEN: usize = 64;
/// Maximum length after placeholder expansion. Bounds the amplification of a
/// bounded literal that contains many placeholders.
const MAX_SERVICE_EXPANDED_LEN: usize = 16 * 1024;
/// Maximum entries observed under each brewrs-owned service state root.
const MAX_SERVICE_LIST_ENTRIES: usize = 128;

/// Service keys that this preview interprets for display. Every other declared
/// key is reported as unmodelled rather than silently ignored.
const MODELLED_SERVICE_KEYS: [&str; 7] = [
    "run",
    "run_type",
    "environment_variables",
    "working_dir",
    "name",
    "log_path",
    "error_log_path",
];

/// Preview or list a service without starting or registering anything.
pub(crate) async fn preview(config: &Config, prefix: Option<&Path>, package: &str) -> Result<()> {
    let requested = plan::parse_target(package)?.to_string();
    if requested == "colima" {
        return preview_colima(config, prefix);
    }
    preview_formula(config, prefix, &requested).await
}

fn preview_colima(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let store = Store::open_existing(&prefix_path(prefix)?)
        .context("Colima service preview requires an initialized brewrs prefix")?;
    let prefix = store.prefix().canonicalize()?;
    let retained = store.retained()?;
    let plan = colima_plan(&prefix, &store.installed()?, &retained)?;
    let ui = DnfStyleFormatter::new(config);
    let args: Vec<String> = COLIMA_ARGS.iter().map(|arg| (*arg).to_string()).collect();
    println!("{}", ui.section("Colima service trial (preview only)"));
    for (label, value) in [
        ("Proposed label", plan.label),
        ("Colima", plan.colima.display().to_string()),
        ("Lima", plan.lima.display().to_string()),
        ("Arguments", render_argv(&args, 0)),
        ("Trial HOME", plan.home.display().to_string()),
        ("Working directory", plan.home.display().to_string()),
        ("COLIMA_HOME", plan.colima_home.display().to_string()),
        (
            "COLIMA_CACHE_HOME",
            plan.home.join("cache").display().to_string(),
        ),
        ("COLIMA_PROFILE", plan.profile),
        ("LIMA_HOME", plan.lima_home.display().to_string()),
        ("PATH", plan.path),
    ] {
        println!("{}", ui.field(label, &value));
    }
    println!(
        "\nNothing was started or registered. This plan does not import existing VM data or change your Docker context."
    );
    println!("VM startup and socket paths have not been tested by this preview.");
    println!(
        "The trial HOME, working directory, and environment above are proposals for a future launch; this preview enforces no sandbox or isolation boundary."
    );
    Ok(())
}

struct ColimaPlan {
    label: String,
    profile: String,
    colima: PathBuf,
    lima: PathBuf,
    home: PathBuf,
    colima_home: PathBuf,
    lima_home: PathBuf,
    path: String,
}

fn colima_plan(prefix: &Path, installed: &[Receipt], retained: &[Receipt]) -> Result<ColimaPlan> {
    validate_dependencies_with_retained(installed, &[], retained)
        .context("Cannot preview a service with unsatisfied receipt pins")?;
    let colima = installed
        .iter()
        .find(|receipt| receipt.name == "colima")
        .context("Install colima in this brewrs prefix before previewing its service")?;
    // Match the actual command-line contract reviewed for the initial lane.
    if colima.version != COLIMA_VERSION {
        bail!(
            "A Colima service trial has not been reviewed for the installed version {}",
            colima.version
        );
    }
    let pin = colima
        .dependencies
        .iter()
        .find(|pin| pin.name == "lima")
        .context("The Colima receipt does not pin a managed Lima keg")?;
    let colima_bin = managed_binary(prefix, &colima.name, &colima.version, "colima")?;
    let lima_bin = managed_binary(prefix, &pin.name, &pin.version, "limactl")?;
    let trial = prefix.join(SERVICE_TRIAL_ROOT).join("colima");
    ensure_fresh_trial(&trial)?;
    let home = trial.join("home");
    let colima_home = home.join(".colima");
    // Colima itself can place Lima's state under its own configuration tree.
    let lima_home = colima_home.join("_lima");
    let lima_bin_dir = lima_bin.parent().context("Lima executable has no parent")?;
    let path = format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin",
        displayable_path(lima_bin_dir)?
    );
    for path in [prefix, &home, &colima_bin, &lima_bin] {
        displayable_path(path)?;
    }
    let hash = prefix_hash(prefix)?;
    Ok(ColimaPlan {
        label: local_label(&hash, "colima"),
        profile: format!("brewrs-{}", &hash[..8]),
        colima: colima_bin,
        lima: lima_bin,
        home,
        colima_home,
        lima_home,
        path,
    })
}

async fn preview_formula(config: &Config, prefix: Option<&Path>, requested: &str) -> Result<()> {
    let store = Store::open_existing(&prefix_path(prefix)?).with_context(|| {
        format!("Service preview for '{requested}' requires an initialized brewrs prefix")
    })?;
    let prefix = store.prefix().canonicalize()?;
    let installed = store.installed()?;
    let retained = store.retained()?;
    validate_dependencies_with_retained(&installed, &[], &retained)
        .context("Cannot preview a service with unsatisfied receipt pins")?;
    let receipt = installed
        .iter()
        .find(|receipt| receipt.name == requested)
        .with_context(|| format!("'{requested}' is not installed in this brewrs prefix"))?;
    let tag = default_target()?;
    if !plan::is_supported_macos_tag(&tag) {
        bail!("Service previews are only supported on a macOS host tag, not '{tag}'");
    }
    let metadata = fetch_installed_service(
        config,
        &receipt.name,
        &tag,
        &receipt.version,
        receipt.version_scheme,
        &receipt.sha256,
    )
    .await?;
    let plan = ServicePreview::from_metadata(&prefix, &receipt.name, &receipt.version, &metadata)?;
    render_preview(config, &plan);
    Ok(())
}

/// A fully validated, read-only service preview.
///
/// `declared_argv` preserves the official literal metadata. `expanded_argv`
/// expands only the two Homebrew path placeholders this preview models, and
/// `executable` is always the pinned keg path rather than a mutable `opt` alias.
struct ServicePreview {
    name: String,
    version: String,
    label: String,
    service_name: Option<String>,
    declared_argv: Vec<String>,
    expanded_argv: Vec<String>,
    executable: PathBuf,
    used_opt_alias: bool,
    working_directory: Option<(PathBuf, DirectoryObservation)>,
    environment: Vec<(String, String)>,
    log_path: Option<PathBuf>,
    error_log_path: Option<PathBuf>,
    run_type: Option<String>,
    unmodelled: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryObservation {
    Present,
    Absent,
    OutsidePrefix,
}

impl ServicePreview {
    fn from_metadata(
        prefix: &Path,
        name: &str,
        version: &str,
        metadata: &InstalledServiceMetadata,
    ) -> Result<Self> {
        if metadata.name != name || metadata.version != version {
            bail!(
                "service metadata for {} {} does not match the pinned receipt for {name} {version}",
                metadata.name,
                metadata.version
            );
        }
        let service = metadata
            .service
            .as_object()
            .context("Service declaration is not an object")?;
        for key in service.keys() {
            let valid = !key.is_empty()
                && key.len() <= MAX_SERVICE_KEY_LEN
                && key
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_');
            if !valid {
                bail!("Service declaration has an unsupported key {key:?}");
            }
        }
        if let Some(root) = service.get("require_root") {
            match root {
                Value::Bool(true) => bail!(
                    "Formula '{name}' declares a service that requires root; this preview refuses privileged services"
                ),
                Value::Bool(false) => {}
                _ => bail!("Service require_root must be a boolean"),
            }
        }

        let declared = macos_run_args(service)?;
        if declared.is_empty() || declared.len() > MAX_SERVICE_ARGS {
            bail!(
                "Service run declares {} arguments, outside the supported 1..={MAX_SERVICE_ARGS} range",
                declared.len()
            );
        }
        let mut declared_argv = Vec::with_capacity(declared.len());
        let mut expanded_argv = Vec::with_capacity(declared.len());
        for (index, value) in declared.iter().enumerate() {
            let value = value
                .as_str()
                .with_context(|| format!("Service run argument {index} is not a string"))?;
            let what = format!("run argument {index}");
            review_literal(value, &what)?;
            declared_argv.push(value.to_string());
            expanded_argv.push(expand_literal(prefix, value, &what)?);
        }
        let (executable, used_opt_alias) =
            pinned_executable(prefix, name, version, &expanded_argv[0])?;

        let working_directory = match service.get("working_dir") {
            None => None,
            Some(Value::String(raw)) => {
                let expanded = expand_literal(prefix, raw, "working_dir")?;
                let path = absolute_path(&expanded, "working_dir")?;
                let observation = observe_directory(prefix, &path)?;
                Some((path, observation))
            }
            Some(_) => bail!("Service working_dir must be a string"),
        };

        let environment = match service.get("environment_variables") {
            None => Vec::new(),
            Some(Value::Object(variables)) => {
                if variables.len() > MAX_SERVICE_ENV {
                    bail!(
                        "Service declares {} environment variables, above the {MAX_SERVICE_ENV} limit",
                        variables.len()
                    );
                }
                let mut environment = Vec::with_capacity(variables.len());
                for (key, value) in variables {
                    validate_environment_name(key)?;
                    let value = value.as_str().with_context(|| {
                        format!("Service environment variable {key} is not a string")
                    })?;
                    let expanded =
                        expand_literal(prefix, value, &format!("environment variable {key}"))?;
                    environment.push((key.clone(), expanded));
                }
                environment.sort_by(|left, right| left.0.cmp(&right.0));
                environment
            }
            Some(_) => bail!("Service environment_variables must be an object"),
        };

        let service_name = match service.get("name") {
            None => None,
            Some(value) => macos_name(value)?,
        };
        let run_type = match service.get("run_type") {
            None => None,
            Some(Value::String(value)) => {
                review_literal(value, "run_type")?;
                if value.len() > MAX_RUN_TYPE_LEN {
                    bail!("Service run_type is longer than the {MAX_RUN_TYPE_LEN}-character limit");
                }
                Some(value.clone())
            }
            Some(_) => bail!("Service run_type must be a string"),
        };
        let log_path = match service.get("log_path") {
            None => None,
            Some(Value::String(raw)) => {
                let expanded = expand_literal(prefix, raw, "log_path")?;
                Some(absolute_path(&expanded, "log_path")?)
            }
            Some(_) => bail!("Service log_path must be a string"),
        };
        let error_log_path = match service.get("error_log_path") {
            None => None,
            Some(Value::String(raw)) => {
                let expanded = expand_literal(prefix, raw, "error_log_path")?;
                Some(absolute_path(&expanded, "error_log_path")?)
            }
            Some(_) => bail!("Service error_log_path must be a string"),
        };

        let mut unmodelled: Vec<String> = service
            .keys()
            .filter(|key| !MODELLED_SERVICE_KEYS.contains(&key.as_str()))
            .cloned()
            .collect();
        unmodelled.sort_unstable();

        Ok(Self {
            name: name.to_string(),
            version: version.to_string(),
            label: local_label(&prefix_hash(prefix)?, name),
            service_name,
            declared_argv,
            expanded_argv,
            executable,
            used_opt_alias,
            working_directory,
            environment,
            log_path,
            error_log_path,
            run_type,
            unmodelled,
        })
    }
}

fn render_preview(config: &Config, plan: &ServicePreview) {
    let ui = DnfStyleFormatter::new(config);
    let working_directory = match &plan.working_directory {
        None => "not declared".to_string(),
        Some((path, DirectoryObservation::Present)) => {
            format!("{} (declared; not created)", path.display())
        }
        Some((path, DirectoryObservation::Absent)) => {
            format!("{} (declared; not present; not created)", path.display())
        }
        Some((path, DirectoryObservation::OutsidePrefix)) => format!(
            "{} (declared outside the brewrs prefix; not inspected)",
            path.display()
        ),
    };
    let pinned = if plan.used_opt_alias {
        format!(
            "{} (declared via an opt alias verified against the pinned keg)",
            plan.executable.display()
        )
    } else {
        plan.executable.display().to_string()
    };
    let arguments = if plan.expanded_argv.len() > 1 {
        render_argv(&plan.expanded_argv[1..], 1)
    } else {
        "(none)".to_string()
    };
    let environment = if plan.environment.is_empty() {
        "none declared".to_string()
    } else {
        plan.environment
            .iter()
            .map(|(name, value)| format!("{name}={value:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let declared_name = plan.service_name.as_ref().map_or_else(
        || "not declared".to_string(),
        |name| format!("{name} (Homebrew namespace; not used or registered)"),
    );
    println!(
        "{}",
        ui.section(&format!("Service preview: {} {}", plan.name, plan.version))
    );
    for (label, value) in [
        ("Package", format!("{} {}", plan.name, plan.version)),
        ("Proposed label", format!("{} (not registered)", plan.label)),
        ("Declared name", declared_name),
        ("Declared argv", render_argv(&plan.declared_argv, 0)),
        ("Pinned executable", pinned),
        ("Arguments", arguments),
        ("Working directory", working_directory),
        ("Declared environment", environment),
        (
            "Log path",
            plan.log_path.as_ref().map_or_else(
                || "not declared".to_string(),
                |path| format!("{} (declared; not created)", path.display()),
            ),
        ),
        (
            "Error log path",
            plan.error_log_path.as_ref().map_or_else(
                || "not declared".to_string(),
                |path| format!("{} (declared; not created)", path.display()),
            ),
        ),
        (
            "Run type",
            plan.run_type
                .clone()
                .unwrap_or_else(|| "not declared".to_string()),
        ),
        (
            "Unmodelled metadata",
            if plan.unmodelled.is_empty() {
                "none".to_string()
            } else {
                plan.unmodelled.join(", ")
            },
        ),
    ] {
        println!("{}", ui.field(label, &value));
    }
    println!(
        "\nNothing was started, registered, or written. This preview creates no directory or cache, runs no binary, and reads no launchd, process, socket, or Homebrew service state."
    );
    println!(
        "The command and environment above are proposed metadata; environment values expand only $HOMEBREW_PREFIX and $HOMEBREW_CELLAR. No sandbox or isolation boundary is enforced, and no shell environment is inherited."
    );
    println!(
        "Only the executable is verified and pinned to the receipt's keg; every other argument and path is shown as declared and is not pinned, created, or executed."
    );
}

/// List only what can be observed locally about brewrs-managed services.
///
/// This never invokes `launchctl`, reads a process table, opens a socket, or
/// inspects `~/Library/LaunchAgents` or Homebrew's `homebrew.mxcl.*` plists.
/// A state directory is reported as present or absent; it is never translated
/// into a running, loaded, or installed-service claim.
pub(crate) fn list(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let owned = store.is_owned();
    let receipts = store.installed()?;
    if owned {
        let retained = store.retained()?;
        validate_dependencies_with_retained(&receipts, &[], &retained)
            .context("Cannot list services with unsatisfied receipt pins")?;
    }
    let prefix = store.prefix();
    let observed = observe_state(prefix, owned)?;
    let ui = DnfStyleFormatter::new(config);

    println!("{}", ui.section("Services"));
    println!("{}", ui.field("Prefix", &prefix.display().to_string()));
    for root in &observed.roots {
        let status = if !root.inspected {
            "not inspected (prefix is not an initialized brewrs store)".to_string()
        } else if root.present {
            format!(
                "present; {} {}",
                root.entries,
                if root.entries == 1 {
                    "entry"
                } else {
                    "entries"
                }
            )
        } else {
            "absent".to_string()
        };
        println!(
            "{}",
            ui.field(root.label, &format!("{} ({status})", root.path.display()))
        );
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut attributed: Vec<(&'static str, String)> = Vec::new();
    if let Some(receipt) = receipts.iter().find(|receipt| receipt.name == "colima") {
        let preview = if receipt.version != COLIMA_VERSION {
            "no reviewed preview for this version"
        } else if !receipt.dependencies.iter().any(|pin| pin.name == "lima") {
            "no reviewed preview (Lima pin missing)"
        } else {
            "reviewed trial (0.10.3)"
        };
        let state = observed.describe("colima", &mut attributed);
        rows.push(vec![
            "colima".to_string(),
            receipt.version.clone(),
            preview.to_string(),
            state,
        ]);
    }
    let mut unattributed: std::collections::BTreeMap<String, Vec<&ObservedEntry>> =
        std::collections::BTreeMap::new();
    for entry in &observed.entries {
        if attributed
            .iter()
            .any(|(root, name)| *root == entry.root && *name == entry.name)
        {
            continue;
        }
        unattributed
            .entry(entry.name.clone())
            .or_default()
            .push(entry);
    }
    for (name, entries) in unattributed {
        let mut state = Vec::with_capacity(entries.len());
        for entry in entries {
            let root = if entry.root == "service-state" {
                SERVICE_STATE_ROOT
            } else {
                SERVICE_TRIAL_ROOT
            };
            state.push(format!(
                "{} under {root} (runtime not inspected)",
                entry.kind
            ));
        }
        rows.push(vec![
            name,
            "unknown".to_string(),
            "no metadata recorded".to_string(),
            state.join("; "),
        ]);
    }

    if rows.is_empty() {
        println!("\nNo local service state or reviewed Colima receipt observed.");
    } else {
        println!();
        println!(
            "{}",
            ui.table(
                &format!("Services ({})", rows.len()),
                &["Service", "Version", "Preview", "Brewrs state"],
                &rows
            )
        );
    }
    println!(
        "\nService lifecycle is not implemented: nothing is started, stopped, registered, or supervised."
    );
    println!(
        "This list reads only the two brewrs state roots above; it does not read launchd, processes, sockets, ~/Library/LaunchAgents, or Homebrew's homebrew.mxcl.* plists."
    );
    println!(
        "Receipts do not record formula service declarations; use `brewrs services plan <formula>` to preview an installed formula from official metadata pinned to its receipt."
    );
    Ok(())
}

struct ObservedState {
    roots: Vec<StateRoot>,
    entries: Vec<ObservedEntry>,
}

struct StateRoot {
    label: &'static str,
    path: PathBuf,
    present: bool,
    entries: usize,
    inspected: bool,
}

struct ObservedEntry {
    root: &'static str,
    name: String,
    kind: &'static str,
}

impl ObservedState {
    fn describe(&self, name: &str, attributed: &mut Vec<(&'static str, String)>) -> String {
        let mut parts = Vec::new();
        for entry in self.entries.iter().filter(|entry| entry.name == name) {
            attributed.push((entry.root, entry.name.clone()));
            let root = if entry.root == "service-state" {
                SERVICE_STATE_ROOT
            } else {
                SERVICE_TRIAL_ROOT
            };
            parts.push(format!(
                "{root} {} present (runtime not inspected)",
                entry.kind
            ));
        }
        if parts.is_empty() {
            "no brewrs service state".to_string()
        } else {
            parts.join("; ")
        }
    }
}

fn observe_state(prefix: &Path, inspected: bool) -> Result<ObservedState> {
    let mut roots = Vec::with_capacity(2);
    let mut entries = Vec::new();
    for (key, label, relative) in [
        ("service-state", "Service state", SERVICE_STATE_ROOT),
        ("service-trial", "Service trials", SERVICE_TRIAL_ROOT),
    ] {
        let path = prefix.join(relative);
        if !inspected {
            roots.push(StateRoot {
                label,
                path,
                present: false,
                entries: 0,
                inspected: false,
            });
            continue;
        }
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                roots.push(StateRoot {
                    label,
                    path,
                    present: false,
                    entries: 0,
                    inspected: true,
                });
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Could not inspect {}", path.display()));
            }
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "Refusing to list '{}': a brewrs service state root must not be a symlink",
                        path.display()
                    );
                }
                if !metadata.is_dir() {
                    bail!(
                        "Refusing to list '{}': a brewrs service state root must be a directory",
                        path.display()
                    );
                }
                let mut names = Vec::new();
                for entry in fs::read_dir(&path)
                    .with_context(|| format!("Could not read {}", path.display()))?
                {
                    let entry = entry.with_context(|| {
                        format!("Could not read an entry in {}", path.display())
                    })?;
                    let entry_path = entry.path();
                    let metadata = fs::symlink_metadata(&entry_path)
                        .with_context(|| format!("Could not inspect {}", entry_path.display()))?;
                    let kind = if metadata.file_type().is_symlink() {
                        "symlink (not followed)"
                    } else if metadata.is_dir() {
                        "directory"
                    } else if metadata.is_file() {
                        "file"
                    } else {
                        "special file"
                    };
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.is_empty()
                        || name.len() > MAX_SERVICE_NAME_LEN
                        || name.chars().any(char::is_control)
                    {
                        bail!(
                            "Refusing to list '{}': an entry has an unsupported name",
                            path.display()
                        );
                    }
                    names.push((name, kind));
                    if names.len() > MAX_SERVICE_LIST_ENTRIES {
                        bail!(
                            "Refusing to list '{}': more than {MAX_SERVICE_LIST_ENTRIES} entries",
                            path.display()
                        );
                    }
                }
                names.sort_by(|left, right| left.0.cmp(&right.0));
                roots.push(StateRoot {
                    label,
                    path,
                    present: true,
                    entries: names.len(),
                    inspected: true,
                });
                entries.extend(names.into_iter().map(|(name, kind)| ObservedEntry {
                    root: key,
                    name,
                    kind,
                }));
            }
        }
    }
    Ok(ObservedState { roots, entries })
}

fn ensure_fresh_trial(trial: &Path) -> Result<()> {
    let parents = trial.parent().context("Trial has no parent directory")?;
    match fs::symlink_metadata(parents) {
        Ok(meta) if meta.is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => bail!(
            "Service trial parent is not a managed directory: {}",
            parents.display()
        ),
        Err(error) => return Err(error).context("Could not inspect the service trial parent"),
    }
    match fs::symlink_metadata(trial) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Could not inspect the service trial directory"),
        Ok(_) => bail!(
            "Service trial path already exists; choose a fresh brewrs prefix to preview a new side-by-side trial"
        ),
    }
}

fn managed_binary(prefix: &Path, name: &str, version: &str, bin: &str) -> Result<PathBuf> {
    let cellar = prefix.join("Cellar");
    let package = cellar.join(name);
    let keg = package.join(version);
    for path in [&cellar, &package, &keg, &keg.join("bin")] {
        if !fs::symlink_metadata(path)
            .with_context(|| format!("Could not inspect {}", path.display()))?
            .is_dir()
        {
            bail!("Not a real managed directory: {}", path.display());
        }
    }
    let path = keg.join("bin").join(bin);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Could not inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "Service requires a regular managed executable: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("Service binary is not executable: {}", path.display());
        }
    }
    Ok(path)
}

/// Extract the macOS argv from a validated `service.run` declaration.
///
/// A shell command string is refused because this preview never parses a shell
/// command line; only an explicit argv array is rendered.
fn macos_run_args(service: &Map<String, Value>) -> Result<&[Value]> {
    let run = service
        .get("run")
        .context("Service declaration has no run command")?;
    let value = match run {
        Value::Array(_) => run,
        Value::Object(os_map) => os_map
            .get("macos")
            .context("Service run declaration has no macOS entry")?,
        Value::String(_) => {
            bail!("Service run is a shell command string; this preview only models argv arrays")
        }
        _ => bail!("Service run has an unsupported shape"),
    };
    match value {
        Value::Array(args) => Ok(args),
        Value::String(_) => {
            bail!("Service run is a shell command string; this preview only models argv arrays")
        }
        _ => bail!("Service run must be an argv array"),
    }
}

fn macos_name(value: &Value) -> Result<Option<String>> {
    let name = match value {
        Value::String(name) => Some(name),
        Value::Object(os_map) => match os_map.get("macos") {
            Some(Value::String(name)) => Some(name),
            Some(_) => bail!("Service name macOS entry must be a string"),
            None => None,
        },
        _ => bail!("Service name must be a string or an OS map"),
    };
    match name {
        None => Ok(None),
        Some(name) => {
            if name.is_empty()
                || name.len() > MAX_SERVICE_NAME_LEN
                || !name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
                })
            {
                bail!("Service name is not a bounded launchd label");
            }
            Ok(Some(name.clone()))
        }
    }
}

fn validate_environment_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid = name.len() <= MAX_SERVICE_ENV_NAME_LEN
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
    if !valid {
        bail!("Service environment variable {name:?} is not a bounded variable name");
    }
    Ok(())
}

/// Bound one untrusted literal before it is rendered or interpreted.
fn review_literal(value: &str, what: &str) -> Result<()> {
    if value.is_empty() {
        bail!("Service {what} must not be empty");
    }
    if value.len() > MAX_SERVICE_LITERAL_LEN {
        bail!("Service {what} is longer than the {MAX_SERVICE_LITERAL_LEN}-byte limit");
    }
    if value.chars().any(char::is_control) {
        bail!("Service {what} contains a control character");
    }
    Ok(())
}

/// Expand only the two Homebrew path placeholders modelled by this preview.
///
/// Substitution is a single left-to-right scan of the original literal: text
/// inserted from the prefix is never rescanned, so a prefix that itself
/// contains a placeholder-looking string cannot be expanded twice. An unbraced
/// placeholder must end at a token boundary, so `$HOMEBREW_PREFIX_UNKNOWN` is
/// refused instead of being read as the prefix followed by text. Any other `$`
/// reference is refused rather than resolved from the ambient environment, so
/// no environment inheritance is implied.
fn expand_literal(prefix: &Path, value: &str, what: &str) -> Result<String> {
    review_literal(value, what)?;
    let prefix_text = displayable_path(prefix)?;
    let cellar = prefix.join("Cellar");
    let cellar_text = displayable_path(&cellar)?;
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        expanded.push_str(&rest[..index]);
        let candidate = &rest[index..];
        let (replacement, consumed) = if let Some(braced) = candidate.strip_prefix("${") {
            if braced.starts_with("HOMEBREW_PREFIX}") {
                (prefix_text, "${HOMEBREW_PREFIX}".len())
            } else if braced.starts_with("HOMEBREW_CELLAR}") {
                (cellar_text, "${HOMEBREW_CELLAR}".len())
            } else {
                bail!("{}", unsupported_variable(what, value));
            }
        } else if let Some(tail) = candidate.strip_prefix("$HOMEBREW_PREFIX") {
            if starts_variable_name(tail) {
                bail!("{}", unsupported_variable(what, value));
            }
            (prefix_text, "$HOMEBREW_PREFIX".len())
        } else if let Some(tail) = candidate.strip_prefix("$HOMEBREW_CELLAR") {
            if starts_variable_name(tail) {
                bail!("{}", unsupported_variable(what, value));
            }
            (cellar_text, "$HOMEBREW_CELLAR".len())
        } else {
            bail!("{}", unsupported_variable(what, value));
        };
        if expanded.len() + replacement.len() > MAX_SERVICE_EXPANDED_LEN {
            bail!("Service {what} expands beyond the {MAX_SERVICE_EXPANDED_LEN}-byte limit");
        }
        expanded.push_str(replacement);
        rest = &candidate[consumed..];
    }
    if expanded.len() + rest.len() > MAX_SERVICE_EXPANDED_LEN {
        bail!("Service {what} expands beyond the {MAX_SERVICE_EXPANDED_LEN}-byte limit");
    }
    expanded.push_str(rest);
    Ok(expanded)
}

fn starts_variable_name(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn unsupported_variable(what: &str, value: &str) -> String {
    format!(
        "Service {what} uses a variable this preview does not model (only $HOMEBREW_PREFIX and $HOMEBREW_CELLAR are expanded): {value:?}"
    )
}

/// Require an absolute, lexically normalized path with no `.`/`..` components.
fn absolute_path(value: &str, what: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("Service {what} must be an absolute path after expansion: {value:?}");
    }
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => {
                bail!("Service {what} must not contain '.' or '..' path components: {value:?}")
            }
            _ => bail!("Service {what} has an unsupported path prefix: {value:?}"),
        }
    }
    Ok(path.to_path_buf())
}

/// Resolve the declared executable to the pinned keg and verify every ancestor.
///
/// A `$HOMEBREW_PREFIX/opt/<name>` declaration is accepted only when the opt
/// entry is a real symlink whose target is exactly the pinned keg; the returned
/// path is always the immutable keg path.
fn pinned_executable(
    prefix: &Path,
    name: &str,
    version: &str,
    value: &str,
) -> Result<(PathBuf, bool)> {
    let path = absolute_path(value, "run argument 0")?;
    let keg = prefix.join("Cellar").join(name).join(version);
    let (relative, used_opt_alias) = match path.strip_prefix(&keg) {
        Ok(relative) => (relative.to_path_buf(), false),
        Err(_) => {
            let opt = prefix.join("opt").join(name);
            let Ok(relative) = path.strip_prefix(&opt) else {
                bail!(
                    "Service executable is outside the pinned keg: {} (pinned keg {})",
                    path.display(),
                    keg.display()
                );
            };
            verify_opt_alias(prefix, name, version)?;
            (relative.to_path_buf(), true)
        }
    };
    if relative.as_os_str().is_empty() {
        bail!("Service executable is the pinned keg directory, not a file");
    }
    let executable = keg.join(&relative);
    verify_keg_ancestors(prefix, &executable)?;
    verify_executable(&executable)?;
    Ok((executable, used_opt_alias))
}

/// Verify the reviewed brewrs opt link without resolving it.
///
/// The raw target must be exactly `../Cellar/<name>/<version>`, the only link
/// format brewrs creates. Lexical normalization is deliberately not used: a
/// target such as `../Cellar/current/0.46.0` could point through an arbitrary
/// symlinked intermediate and still normalize to the keg. The keg itself is
/// then checked as a real, non-symlinked directory chain by
/// [`verify_keg_ancestors`].
fn verify_opt_alias(prefix: &Path, name: &str, version: &str) -> Result<()> {
    let opt = prefix.join("opt");
    ensure_real_directory(&opt, "managed opt directory")?;
    let alias = opt.join(name);
    let metadata = fs::symlink_metadata(&alias)
        .with_context(|| format!("Could not inspect {}", alias.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!(
            "Service executable uses '{}', but it is not a managed symlink",
            alias.display()
        );
    }
    let target = fs::read_link(&alias)
        .with_context(|| format!("Could not read the symlink {}", alias.display()))?;
    let expected = Path::new("..").join("Cellar").join(name).join(version);
    if target != expected {
        bail!(
            "Service executable uses the mutable opt alias '{}', but its target {} is not the exact pinned keg link {expected:?}",
            alias.display(),
            target.display()
        );
    }
    Ok(())
}

fn verify_keg_ancestors(prefix: &Path, executable: &Path) -> Result<()> {
    let relative = executable.strip_prefix(prefix).with_context(|| {
        format!(
            "Service executable is outside the brewrs prefix: {}",
            executable.display()
        )
    })?;
    let mut current = prefix.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!(
                "Service executable has an unsupported path component: {}",
                executable.display()
            );
        };
        current.push(part);
        if current == executable {
            break;
        }
        ensure_real_directory(&current, "service executable ancestor")?;
    }
    Ok(())
}

fn verify_executable(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "Service requires a regular executable file, not a symlink: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("Service binary is not executable: {}", path.display());
        }
    }
    Ok(())
}

fn ensure_real_directory(path: &Path, what: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{what} is not a real directory: {}", path.display());
    }
    Ok(())
}

/// Observe a working directory without creating or following anything.
///
/// Inside the prefix every existing component must be a real directory; a
/// missing component means the declared directory is not present yet. Paths
/// outside the prefix are never inspected.
fn observe_directory(prefix: &Path, path: &Path) -> Result<DirectoryObservation> {
    let Ok(relative) = path.strip_prefix(prefix) else {
        return Ok(DirectoryObservation::OutsidePrefix);
    };
    let mut current = prefix.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!(
                "Service working_dir has an unsupported path component: {}",
                path.display()
            );
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "Service working_dir is not a real directory: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DirectoryObservation::Absent);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Could not inspect {}", current.display()));
            }
        }
    }
    Ok(DirectoryObservation::Present)
}

fn prefix_hash(prefix: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(
        displayable_path(prefix)?.as_bytes(),
    )))
}

/// Keep the reviewed Colima namespace shape: `local.brewrs.<hash>.<suffix>`.
/// Characters that launchd labels do not accept are replaced in the suffix.
fn local_label(hash: &str, name: &str) -> String {
    let suffix: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!("local.brewrs.{}.{suffix}", &hash[..16])
}

fn render_argv(args: &[String], offset: usize) -> String {
    args.iter()
        .enumerate()
        .map(|(index, argument)| format!("[{}] {argument:?}", index + offset))
        .collect::<Vec<_>>()
        .join("\n")
}

fn displayable_path(path: &Path) -> Result<&str> {
    let value = path
        .to_str()
        .context("Service preview requires UTF-8 paths")?;
    if value.contains(':') || value.chars().any(char::is_control) {
        bail!("Service preview cannot represent paths containing colons or control characters");
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
