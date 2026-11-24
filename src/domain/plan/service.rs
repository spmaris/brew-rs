//! Service metadata: review of `service` declarations and installed-service verification.

use serde_json::Value;

use super::error::{MetadataError, ServiceError};
use super::macos::is_supported_macos_tag;
use super::model::FormulaResponse;
use super::validate::{
    CORE_TAP, available_tags, validate_formula_name, validate_sha256, validate_version,
};

/// Blocker for a service declaration that is not the recognized optional one.
pub(super) const BLOCKER_SERVICE: &str =
    "declares a service declaration this native installer does not recognize as optional";

/// Service metadata for an installed formula, pinned to its receipt.
///
/// The infrastructure layer's `fetch_installed_service` only returns after the official payload has been
/// proven to describe exactly the installed artifact, so a preview can never
/// render a declaration that belongs to another version or bottle.
pub(crate) struct InstalledServiceMetadata {
    pub name: String,
    pub version: String,
    pub service: serde_json::Value,
}

/// Require official metadata for `name` (already validated with
/// [`validate_formula_name`]) to describe exactly the installed receipt.
///
/// This is a read-only check of a payload fetched without alias resolution or
/// the catalog cache, so rendering a preview never populates or mutates a
/// cache. The tap, stable version (including the `_<revision>` suffix),
/// version scheme, and selected bottle digest must all equal the installed
/// values. Bottle selection follows the planner: the host tag first, then
/// Homebrew's platform-independent `all` bottle. A service declaration is
/// returned only when the existing optional-macOS-service review accepts it.
pub(crate) fn verify_installed_service(
    response: FormulaResponse,
    name: &str,
    tag: &str,
    installed_version: &str,
    installed_version_scheme: u32,
    installed_sha256: &str,
) -> Result<InstalledServiceMetadata, ServiceError> {
    let canonical = validate_formula_name(&response.name, "formula name")?;
    if canonical != name {
        return Err(ServiceError::WrongFormula {
            name: name.to_owned(),
            canonical: canonical.to_owned(),
        });
    }
    if response.tap != CORE_TAP {
        return Err(ServiceError::WrongTap {
            name: name.to_owned(),
            tap: response.tap,
        });
    }
    if response.disabled {
        return Err(ServiceError::Disabled {
            name: name.to_owned(),
        });
    }
    if !response.versions.bottle {
        return Err(ServiceError::NoBottledStable {
            name: name.to_owned(),
        });
    }
    let stable =
        response
            .versions
            .stable
            .as_deref()
            .ok_or_else(|| MetadataError::NoStableVersion {
                name: name.to_owned(),
            })?;
    if stable.is_empty() {
        return Err(MetadataError::EmptyStableVersion {
            name: name.to_owned(),
        }
        .into());
    }
    let revision = response.revision.unwrap_or(0);
    let version = if revision == 0 {
        stable.to_string()
    } else {
        format!("{stable}_{revision}")
    };
    validate_version(&version, name)?;
    if version != installed_version {
        return Err(ServiceError::VersionMismatch {
            name: name.to_owned(),
            installed: installed_version.to_owned(),
            official: version,
        });
    }
    if response.version_scheme != Some(installed_version_scheme) {
        return Err(ServiceError::SchemeMismatch {
            name: name.to_owned(),
            installed: installed_version_scheme,
            official: response.version_scheme,
        });
    }
    let bottle = response
        .bottle
        .as_ref()
        .and_then(|bottle| bottle.stable.as_ref())
        .ok_or_else(|| MetadataError::NoStableBottle {
            name: name.to_owned(),
        })?;
    let bottle_file = bottle
        .files
        .get(tag)
        .or_else(|| bottle.files.get("all"))
        .ok_or_else(|| MetadataError::NoBottleForTag {
            name: name.to_owned(),
            tag: tag.to_owned(),
            available: available_tags(&bottle.files),
        })?;
    let sha256 = validate_sha256(&bottle_file.sha256, name)?;
    if !sha256.eq_ignore_ascii_case(installed_sha256) {
        return Err(ServiceError::DigestMismatch {
            name: name.to_owned(),
            installed: installed_sha256.to_owned(),
            tag: tag.to_owned(),
            official: sha256,
        });
    }
    if response.service.is_null() {
        return Err(ServiceError::NoService {
            name: name.to_owned(),
        });
    }
    if review_service(&response.service, tag).is_some() {
        return Err(ServiceError::UnrecognizedService {
            name: name.to_owned(),
        });
    }
    Ok(InstalledServiceMetadata {
        name: name.to_string(),
        version,
        service: response.service,
    })
}

/// Known keys in an optional Homebrew `service` declaration API payload.
const KNOWN_SERVICE_KEYS: [&str; 18] = [
    "run",
    "run_type",
    "interval",
    "cron",
    "keep_alive",
    "launch_only_once",
    "require_root",
    "environment_variables",
    "working_dir",
    "root_dir",
    "input_path",
    "log_path",
    "error_log_path",
    "macos_legacy_timers",
    "process_type",
    "sockets",
    "name",
    "restart_delay",
];

/// Review a formula's optional `service` declaration against host tag `tag`.
///
/// A service block is metadata, not an install step: Homebrew starts a service
/// only when the user explicitly asks (`brew services start`), never because the
/// package was installed. This planner carries no service state and starts
/// nothing. Valid Homebrew service blocks on supported macOS tags do not block
/// installation; Linux targets and unrecognized shapes keep [`BLOCKER_SERVICE`].
pub(super) fn review_service(service: &Value, tag: &str) -> Option<&'static str> {
    if service.is_null() {
        return None;
    }
    if is_valid_optional_macos_service(service, tag) {
        None
    } else {
        Some(BLOCKER_SERVICE)
    }
}

fn is_valid_optional_macos_service(service: &Value, tag: &str) -> bool {
    if !is_supported_macos_tag(tag) {
        return false;
    }
    let Some(service) = service.as_object() else {
        return false;
    };
    if service.is_empty()
        || service.len() > KNOWN_SERVICE_KEYS.len()
        || !service
            .keys()
            .all(|key| KNOWN_SERVICE_KEYS.contains(&key.as_str()))
    {
        return false;
    }
    if !service.contains_key("run") && !service.contains_key("name") {
        return false;
    }
    if let Some(run) = service.get("run")
        && !is_valid_service_run(run)
    {
        return false;
    }
    true
}

fn is_valid_service_run(run: &Value) -> bool {
    match run {
        Value::String(command) => !command.is_empty(),
        Value::Array(args) => is_valid_service_run_array(args),
        Value::Object(os_map) => {
            !os_map.is_empty()
                && os_map.len() <= 4
                && os_map.iter().all(|(os, value)| {
                    matches!(os.as_str(), "macos" | "linux")
                        && match value {
                            Value::String(command) => !command.is_empty(),
                            Value::Array(args) => is_valid_service_run_array(args),
                            _ => false,
                        }
                })
        }
        _ => false,
    }
}

fn is_valid_service_run_array(args: &[Value]) -> bool {
    !args.is_empty()
        && args.len() <= 64
        && args
            .iter()
            .all(|value| value.as_str().is_some_and(|arg| !arg.is_empty()))
}
