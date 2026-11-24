use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::commands::install::prefix_path;
use crate::config::Config;
use crate::domain::plan;
use crate::domain::plan::InstalledServiceMetadata;
use crate::infra::formula_source::fetch_installed_service;
use crate::infra::platform::default_target;
use crate::infra::store::{Store, validate_dependencies_with_retained};
use crate::ui::DnfStyleFormatter;

use super::executable::pinned_executable;
use super::launchd::{
    MAX_RUN_TYPE_LEN, MAX_SERVICE_ARGS, MAX_SERVICE_ENV, MAX_SERVICE_KEY_LEN,
    MODELLED_SERVICE_KEYS, absolute_path, expand_literal, local_label, macos_name, macos_run_args,
    prefix_hash, render_argv, review_literal, validate_environment_name,
};
use super::observe::{DirectoryObservation, observe_directory};

pub(super) async fn preview_formula(
    config: &Config,
    prefix: Option<&Path>,
    requested: &str,
) -> Result<()> {
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
pub(super) struct ServicePreview {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) label: String,
    pub(super) service_name: Option<String>,
    pub(super) declared_argv: Vec<String>,
    pub(super) expanded_argv: Vec<String>,
    pub(super) executable: PathBuf,
    pub(super) used_opt_alias: bool,
    pub(super) working_directory: Option<(PathBuf, DirectoryObservation)>,
    pub(super) environment: Vec<(String, String)>,
    pub(super) log_path: Option<PathBuf>,
    pub(super) error_log_path: Option<PathBuf>,
    pub(super) run_type: Option<String>,
    pub(super) unmodelled: Vec<String>,
}

impl ServicePreview {
    pub(super) fn from_metadata(
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

pub(super) fn render_preview(config: &Config, plan: &ServicePreview) {
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
