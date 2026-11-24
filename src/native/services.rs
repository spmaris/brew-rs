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
use super::plan::{self, InstalledServiceMetadata};
use super::store::{Receipt, Store, validate_dependencies_with_retained};

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
    let tag = super::default_target()?;
    if !plan::is_supported_macos_tag(&tag) {
        bail!("Service previews are only supported on a macOS host tag, not '{tag}'");
    }
    let metadata = plan::fetch_installed_service(
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
mod tests {
    use super::*;
    use crate::native::store::Dependency;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn seed_colima_prefix(prefix: &Path) -> (PathBuf, PathBuf, Vec<Receipt>) {
        let colima_bin = prefix.join("Cellar/colima/0.10.3/bin/colima");
        let lima_bin = prefix.join("Cellar/lima/2.3.0/bin/limactl");
        for bin in [&colima_bin, &lima_bin] {
            fs::create_dir_all(bin.parent().unwrap()).unwrap();
            fs::write(bin, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let installed = vec![
            Receipt {
                source: Default::default(),
                name: "colima".into(),
                version: "0.10.3".into(),
                version_scheme: 0,
                sha256: "a".repeat(64),
                requested: true,
                keg_only: false,
                conflicts_with: Vec::new(),
                dependencies: vec![Dependency {
                    name: "lima".into(),
                    version: "2.3.0".into(),
                    sha256: "b".repeat(64),
                }],
            },
            Receipt {
                source: Default::default(),
                name: "lima".into(),
                version: "2.3.0".into(),
                version_scheme: 0,
                sha256: "b".repeat(64),
                requested: false,
                keg_only: false,
                conflicts_with: Vec::new(),
                dependencies: Vec::new(),
            },
        ];
        (colima_bin, lima_bin, installed)
    }

    fn seed_keg(prefix: &Path, name: &str, version: &str, member: &str) -> PathBuf {
        let keg = prefix.join("Cellar").join(name).join(version);
        let bin = keg.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join(member);
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(prefix.join("opt")).unwrap();
        let target = PathBuf::from("../Cellar").join(name).join(version);
        std::os::unix::fs::symlink(&target, prefix.join("opt").join(name)).unwrap();
        executable
    }

    fn metadata(service: Value) -> InstalledServiceMetadata {
        InstalledServiceMetadata {
            name: "zellij".into(),
            version: "0.46.0".into(),
            service,
        }
    }

    fn zellij_service() -> Value {
        json!({
            "run": ["$HOMEBREW_PREFIX/opt/zellij/bin/zellij", "web"],
            "run_type": "immediate",
            "keep_alive": { "always": true },
            "environment_variables": {
                "PATH": "$HOMEBREW_PREFIX/bin:$HOMEBREW_PREFIX/sbin:/usr/bin:/bin:/usr/sbin:/sbin"
            },
            "log_path": "$HOMEBREW_PREFIX/var/log/zellij.log",
            "error_log_path": "$HOMEBREW_PREFIX/var/log/zellij.log"
        })
    }

    #[test]
    fn colima_plan_builds_isolated_preview_for_space_bearing_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("Application Support/brewrs");
        fs::create_dir_all(&prefix).unwrap();
        let canonical = prefix.canonicalize().unwrap();
        let (colima_bin, lima_bin, installed) = seed_colima_prefix(&canonical);

        let plan = colima_plan(&canonical, &installed, &[]).unwrap();
        assert!(plan.label.starts_with("local.brewrs."));
        assert!(plan.label.ends_with(".colima"));
        assert_eq!(
            plan.label.len(),
            "local.brewrs.".len() + 16 + ".colima".len()
        );
        assert!(plan.profile.starts_with("brewrs-"));
        assert_eq!(plan.profile.len(), "brewrs-".len() + 8);
        assert_eq!(plan.colima, colima_bin);
        assert_eq!(plan.lima, lima_bin);
        assert_eq!(
            plan.home,
            canonical.join(".brewrs/service-trials/colima/home")
        );
        assert_eq!(plan.colima_home, plan.home.join(".colima"));
        assert_eq!(plan.lima_home, plan.colima_home.join("_lima"));
        assert_eq!(
            plan.path,
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                lima_bin.parent().unwrap().display()
            )
        );
        // Same prefix produces the exact same deterministic plan.
        let again = colima_plan(&canonical, &installed, &[]).unwrap();
        assert_eq!(again.label, plan.label);
        assert_eq!(again.profile, plan.profile);
    }

    #[test]
    fn colima_plan_rejects_unreviewed_versions_missing_pins_and_bad_binaries() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let canonical = prefix.canonicalize().unwrap();
        let (colima_bin, lima_bin, mut installed) = seed_colima_prefix(&canonical);

        // Unreviewed version
        installed[0].version = "0.10.2".into();
        let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
        assert!(err.to_string().contains("0.10.2"));
        installed[0].version = "0.10.3".into();

        // Missing lima pin
        installed[0].dependencies.clear();
        let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
        assert!(err.to_string().contains("Lima"));
        installed[0].dependencies.push(Dependency {
            name: "lima".into(),
            version: "2.3.0".into(),
            sha256: "b".repeat(64),
        });

        // Non-executable binary
        fs::set_permissions(&colima_bin, fs::Permissions::from_mode(0o644)).unwrap();
        let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
        assert!(err.to_string().contains("not executable"));
        fs::set_permissions(&colima_bin, fs::Permissions::from_mode(0o755)).unwrap();

        // Symlinked executable
        let other = temp.path().join("other");
        fs::write(&other, b"#!/bin/sh\n").unwrap();
        fs::remove_file(&lima_bin).unwrap();
        std::os::unix::fs::symlink(&other, &lima_bin).unwrap();
        let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
        assert!(err.to_string().contains("regular managed executable"));
        fs::remove_file(&lima_bin).unwrap();
        fs::write(&lima_bin, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&lima_bin, fs::Permissions::from_mode(0o755)).unwrap();

        // Existing trial directory
        let trial = canonical.join(".brewrs/service-trials/colima");
        fs::create_dir_all(&trial).unwrap();
        let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
        assert!(err.to_string().contains("already exists"));
        fs::remove_dir_all(canonical.join(".brewrs")).unwrap();

        // Symlinked service-trials parent
        fs::create_dir_all(canonical.join(".brewrs")).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, canonical.join(".brewrs/service-trials")).unwrap();
        let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
        assert!(err.to_string().contains("not a managed directory"));
    }

    #[test]
    fn colima_plan_rejects_colon_in_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("bad:prefix");
        fs::create_dir_all(&prefix).unwrap();
        let canonical = prefix.canonicalize().unwrap();
        let (_, _, installed) = seed_colima_prefix(&canonical);

        let err = colima_plan(&canonical, &installed, &[]).err().unwrap();
        assert!(err.to_string().contains("colons"));
    }

    #[test]
    fn general_preview_pins_opt_executable_and_expands_bounded_literals() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("Application Support/brewrs");
        fs::create_dir_all(&prefix).unwrap();
        let canonical = prefix.canonicalize().unwrap();
        let executable = seed_keg(&canonical, "zellij", "0.46.0", "zellij");

        let plan = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .unwrap();
        assert_eq!(plan.executable, executable);
        assert!(plan.used_opt_alias);
        assert_eq!(
            plan.declared_argv,
            [
                "$HOMEBREW_PREFIX/opt/zellij/bin/zellij".to_string(),
                "web".to_string()
            ]
        );
        assert_eq!(
            plan.expanded_argv[0],
            canonical.join("opt/zellij/bin/zellij").to_str().unwrap()
        );
        assert_eq!(plan.expanded_argv[1], "web");
        assert_eq!(
            plan.environment,
            [(
                "PATH".to_string(),
                format!(
                    "{}/bin:{}/sbin:/usr/bin:/bin:/usr/sbin:/sbin",
                    canonical.display(),
                    canonical.display()
                )
            )]
        );
        assert_eq!(plan.unmodelled, ["keep_alive"]);
        assert!(plan.label.starts_with("local.brewrs."));
        assert!(plan.label.ends_with(".zellij"));
        assert_eq!(plan.run_type.as_deref(), Some("immediate"));
        assert_eq!(plan.log_path, Some(canonical.join("var/log/zellij.log")));
    }

    #[test]
    fn general_preview_rejects_a_mutable_opt_alias_and_outside_executables() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let canonical = prefix.canonicalize().unwrap();
        seed_keg(&canonical, "zellij", "0.46.0", "zellij");
        fs::remove_file(canonical.join("opt/zellij")).unwrap();
        std::os::unix::fs::symlink("../Cellar/zellij/0.1.0", canonical.join("opt/zellij")).unwrap();
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("pinned keg"), "{err}");

        fs::remove_file(canonical.join("opt/zellij")).unwrap();
        let mut outside = zellij_service();
        outside["run"] = json!(["$HOMEBREW_PREFIX/bin/zellij", "web"]);
        let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(outside))
            .err()
            .unwrap();
        assert!(err.to_string().contains("outside the pinned keg"), "{err}");
    }

    #[test]
    fn general_preview_rejects_symlinked_ancestors_and_bad_binaries() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let canonical = prefix.canonicalize().unwrap();
        let executable = seed_keg(&canonical, "zellij", "0.46.0", "zellij");

        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).unwrap();
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("not executable"), "{err}");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let outside = temp.path().join("outside");
        fs::write(&outside, b"#!/bin/sh\n").unwrap();
        fs::remove_file(&executable).unwrap();
        std::os::unix::fs::symlink(&outside, &executable).unwrap();
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("not a symlink"), "{err}");
        fs::remove_file(&executable).unwrap();
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let keg = canonical.join("Cellar/zellij/0.46.0");
        let moved = temp.path().join("moved-keg");
        fs::rename(&keg, &moved).unwrap();
        fs::remove_dir(canonical.join("Cellar/zellij")).unwrap();
        std::os::unix::fs::symlink(&moved, canonical.join("Cellar/zellij")).unwrap();
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("not a real directory"), "{err}");
    }

    #[test]
    fn general_preview_rejects_shell_strings_and_linux_only_run_declarations() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        let mut shell = zellij_service();
        shell["run"] = json!("zellij web");
        let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(shell))
            .err()
            .unwrap();
        assert!(err.to_string().contains("shell command string"), "{err}");

        let mut linux = zellij_service();
        linux["run"] = json!({ "linux": ["/usr/bin/zellij", "web"] });
        let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(linux))
            .err()
            .unwrap();
        assert!(err.to_string().contains("no macOS entry"), "{err}");
    }

    #[test]
    fn general_preview_refuses_unknown_placeholders_and_root_services() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        fs::create_dir_all(&canonical).unwrap();
        seed_keg(&canonical, "zellij", "0.46.0", "zellij");
        let mut home = zellij_service();
        home["run"] = json!(["$HOME/bin/zellij", "web"]);
        let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(home))
            .err()
            .unwrap();
        assert!(err.to_string().contains("does not model"), "{err}");

        let mut inherited = zellij_service();
        inherited["environment_variables"] = json!({ "PATH": "$PATH:/usr/bin" });
        let err =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(inherited))
                .err()
                .unwrap();
        assert!(err.to_string().contains("does not model"), "{err}");

        let mut root = zellij_service();
        root["require_root"] = json!(true);
        let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(root))
            .err()
            .unwrap();
        assert!(err.to_string().contains("requires root"), "{err}");
    }

    #[test]
    fn general_preview_notes_every_unmodelled_key() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        seed_keg(&canonical, "zellij", "0.46.0", "zellij");
        let mut service = zellij_service();
        service["process_type"] = json!("Background");
        service["launch_only_once"] = json!(true);
        service["sockets"] = json!({ "Listeners": { "SockPathName": "/tmp/zellij.sock" } });
        let plan =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(service))
                .unwrap();
        assert_eq!(
            plan.unmodelled,
            ["keep_alive", "launch_only_once", "process_type", "sockets"]
        );
    }

    #[test]
    fn general_preview_bounds_arguments_and_environment() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        seed_keg(&canonical, "zellij", "0.46.0", "zellij");

        let mut too_many_args = zellij_service();
        let mut argv = vec![json!("$HOMEBREW_PREFIX/opt/zellij/bin/zellij")];
        argv.extend((0..MAX_SERVICE_ARGS).map(|_| json!("arg")));
        too_many_args["run"] = Value::Array(argv);
        let err =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(too_many_args))
                .err()
                .unwrap();
        assert!(err.to_string().contains("outside the supported"), "{err}");

        let mut long_arg = zellij_service();
        long_arg["run"] = json!([
            "$HOMEBREW_PREFIX/opt/zellij/bin/zellij",
            "x".repeat(MAX_SERVICE_LITERAL_LEN + 1)
        ]);
        let err =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(long_arg))
                .err()
                .unwrap();
        assert!(err.to_string().contains("longer than"), "{err}");

        let mut too_many_variables = zellij_service();
        let variables: Map<String, Value> = (0..=MAX_SERVICE_ENV)
            .map(|index| (format!("VAR_{index}"), json!("value")))
            .collect();
        too_many_variables["environment_variables"] = Value::Object(variables);
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(too_many_variables),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("environment variables"), "{err}");
    }

    #[test]
    fn general_preview_observes_working_directories_without_creating_them() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        seed_keg(&canonical, "zellij", "0.46.0", "zellij");

        let mut present = zellij_service();
        present["working_dir"] = json!("$HOMEBREW_PREFIX");
        let plan =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(present))
                .unwrap();
        assert_eq!(
            plan.working_directory,
            Some((canonical.clone(), DirectoryObservation::Present))
        );

        let mut absent = zellij_service();
        absent["working_dir"] = json!("$HOMEBREW_PREFIX/var");
        let plan = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(absent))
            .unwrap();
        assert_eq!(
            plan.working_directory,
            Some((canonical.join("var"), DirectoryObservation::Absent))
        );
        assert!(!canonical.join("var").exists());

        let mut outside = zellij_service();
        outside["working_dir"] = json!("/tmp");
        let plan =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(outside))
                .unwrap();
        assert_eq!(
            plan.working_directory,
            Some((PathBuf::from("/tmp"), DirectoryObservation::OutsidePrefix))
        );

        let mut escaping = zellij_service();
        escaping["working_dir"] = json!("$HOMEBREW_PREFIX/../escape");
        let err =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(escaping))
                .err()
                .unwrap();
        assert!(err.to_string().contains("must not contain"), "{err}");
    }

    #[test]
    fn general_preview_reads_os_mapped_service_names_but_never_registers_them() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        seed_keg(&canonical, "zellij", "0.46.0", "zellij");

        let mut mapped = zellij_service();
        mapped["name"] = json!({ "macos": "homebrew.mxcl.zellij" });
        let plan = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(mapped))
            .unwrap();
        assert_eq!(plan.service_name.as_deref(), Some("homebrew.mxcl.zellij"));

        let mut linux_name = zellij_service();
        linux_name["name"] = json!({ "linux": "zellij.service" });
        let plan =
            ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(linux_name))
                .unwrap();
        assert_eq!(plan.service_name, None);

        let mut invalid = zellij_service();
        invalid["name"] = json!("bad name");
        let err = ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(invalid))
            .err()
            .unwrap();
        assert!(err.to_string().contains("launchd label"), "{err}");
    }

    #[test]
    fn general_preview_requires_the_exact_relative_opt_link() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let canonical = prefix.canonicalize().unwrap();
        seed_keg(&canonical, "zellij", "0.46.0", "zellij");
        let alias = canonical.join("opt/zellij");
        let keg = canonical.join("Cellar/zellij/0.46.0");

        // An absolute target reads as the same keg but is not the reviewed
        // relative link brewrs writes.
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&keg, &alias).unwrap();
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("exact pinned keg link"), "{err}");

        // Extra `..` segments normalize to the same keg lexically, but the raw
        // target is still not the exact reviewed link.
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink("../Cellar/zellij/0.46.0/../0.46.0", &alias).unwrap();
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("exact pinned keg link"), "{err}");

        // A symlinked intermediate could resolve to the keg too; the raw link
        // format must still be rejected rather than resolved.
        std::os::unix::fs::symlink("zellij", canonical.join("Cellar/current")).unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink("../Cellar/current/0.46.0", &alias).unwrap();
        let err = ServicePreview::from_metadata(
            &canonical,
            "zellij",
            "0.46.0",
            &metadata(zellij_service()),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("exact pinned keg link"), "{err}");
    }

    #[test]
    fn placeholder_expansion_is_single_pass_and_token_bounded() {
        let temp = tempfile::tempdir().unwrap();
        // A prefix that itself contains a placeholder-looking string proves
        // inserted text is not rescanned.
        let prefix = temp.path().join("$HOMEBREW_CELLAR/prefix");
        let prefix_text = prefix.to_str().unwrap();
        assert_eq!(
            expand_literal(&prefix, "$HOMEBREW_PREFIX/bin", "run argument 0").unwrap(),
            prefix.join("bin").to_str().unwrap()
        );
        assert_eq!(
            expand_literal(&prefix, "${HOMEBREW_PREFIX}/bin", "run argument 0").unwrap(),
            prefix.join("bin").to_str().unwrap()
        );
        assert_eq!(
            expand_literal(&prefix, "$HOMEBREW_CELLAR/lib", "run argument 0").unwrap(),
            prefix.join("Cellar/lib").to_str().unwrap()
        );

        // An unbraced name must end at a token boundary.
        let err = expand_literal(&prefix, "$HOMEBREW_PREFIX_UNKNOWN", "run argument 0")
            .err()
            .unwrap();
        assert!(err.to_string().contains("does not model"), "{err}");
        assert!(expand_literal(&prefix, "$HOME/bin", "run argument 0").is_err());
        assert!(expand_literal(&prefix, "${HOMEBREW_REPOSITORY}", "run argument 0").is_err());
        assert!(expand_literal(&prefix, "trailing $", "run argument 0").is_err());

        // Expansion is bounded even when the literal itself is within bounds.
        let long_prefix = Path::new("/tmp").join("x".repeat(2048));
        let value = "$HOMEBREW_PREFIX/".repeat(20);
        let err = expand_literal(&long_prefix, &value, "run argument 0")
            .err()
            .unwrap();
        assert!(err.to_string().contains("expands beyond"), "{err}");
        assert!(prefix_text.contains("$HOMEBREW_CELLAR"));
    }

    #[test]
    fn general_preview_rejects_unbounded_or_control_service_keys() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().canonicalize().unwrap();
        for key in [
            "bad key".to_string(),
            "run\n".to_string(),
            "k".repeat(MAX_SERVICE_KEY_LEN + 1),
        ] {
            let mut service = zellij_service();
            service[key.as_str()] = json!(1);
            let err =
                ServicePreview::from_metadata(&canonical, "zellij", "0.46.0", &metadata(service))
                    .err()
                    .unwrap();
            assert!(
                err.to_string().contains("unsupported key"),
                "{key:?}: {err}"
            );
        }
    }

    #[test]
    fn state_roots_are_observed_without_following_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let observed = observe_state(&prefix, true).unwrap();
        assert!(observed.entries.is_empty());
        assert!(
            observed
                .roots
                .iter()
                .all(|root| root.inspected && !root.present)
        );

        fs::create_dir_all(prefix.join(SERVICE_STATE_ROOT).join("alpha")).unwrap();
        fs::create_dir_all(prefix.join(SERVICE_TRIAL_ROOT)).unwrap();
        fs::write(prefix.join(SERVICE_TRIAL_ROOT).join("colima"), b"x").unwrap();
        let observed = observe_state(&prefix, true).unwrap();
        assert!(observed.roots.iter().all(|root| root.present));
        assert_eq!(
            observed
                .entries
                .iter()
                .map(|entry| (entry.root, entry.name.as_str(), entry.kind))
                .collect::<Vec<_>>(),
            [
                ("service-state", "alpha", "directory"),
                ("service-trial", "colima", "file")
            ]
        );

        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::remove_dir_all(prefix.join(SERVICE_STATE_ROOT)).unwrap();
        std::os::unix::fs::symlink(&outside, prefix.join(SERVICE_STATE_ROOT)).unwrap();
        let err = observe_state(&prefix, true).err().unwrap();
        assert!(err.to_string().contains("symlink"), "{err}");

        let unowned = observe_state(&prefix, false).unwrap();
        assert!(unowned.entries.is_empty());
        assert!(unowned.roots.iter().all(|root| !root.inspected));
    }

    #[test]
    fn state_roots_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let root = prefix.join(SERVICE_STATE_ROOT);
        fs::create_dir_all(&root).unwrap();
        for index in 0..=MAX_SERVICE_LIST_ENTRIES {
            fs::create_dir(root.join(format!("entry-{index:03}"))).unwrap();
        }
        let err = observe_state(&prefix, true).err().unwrap();
        assert!(err.to_string().contains("more than"), "{err}");
    }

    #[tokio::test]
    async fn preview_rejects_packages_that_are_not_installed_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("empty");
        fs::create_dir_all(&prefix).unwrap();
        let err = preview(&Config::default(), Some(&prefix), "zellij")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("initialized brewrs prefix"),
            "{err}"
        );
    }
}
