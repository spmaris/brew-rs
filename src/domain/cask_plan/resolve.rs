//! Cask metadata resolution: the source port, payload parsing and plan assembly.

use std::collections::HashSet;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use super::super::plan::is_supported_macos_tag;
use super::super::validate::{is_lower_hex_digest, validate_cask_url};
use super::artifacts::review_artifacts;
use super::error::CaskPlanError;
use super::identifier::{CASK_TAP, parse_cask_target, validate_cask_identifier};
use super::model::{ContainerHint, PlannedCask};
use super::requirements::{review_conflicts_with, review_depends_on};

/// Maximum cask targets accepted in one invocation.
pub(super) const MAX_TARGETS: usize = 64;

/// Maximum length of a cask version.
const MAX_VERSION_LEN: usize = 128;

/// A source of cask metadata: the port the resolver reads through.
pub(crate) trait CaskSource {
    async fn fetch_cask(&self, token: &str) -> Result<Value>;
}

/// Resolve cask install plans for `targets` on host tag `tag`, reading
/// metadata through `source`.
pub(crate) async fn resolve_with<S: CaskSource>(
    source: &S,
    targets: &[String],
    tag: &str,
) -> Result<Vec<PlannedCask>> {
    if targets.is_empty() {
        return Err(CaskPlanError::NoTargets.into());
    }
    if targets.len() > MAX_TARGETS {
        return Err(CaskPlanError::TooManyTargets {
            count: targets.len(),
        }
        .into());
    }

    let mut seen_tokens = HashSet::new();
    let mut planned = Vec::new();
    for raw_target in targets {
        let token = parse_cask_target(raw_target)?;
        let raw_payload = source.fetch_cask(token).await?;
        let cask = plan_from_value(raw_payload, token, tag)?;
        if seen_tokens.insert(cask.token.clone()) {
            planned.push(cask);
        }
    }
    Ok(planned)
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawCask {
    token: String,
    tap: String,
    name: Vec<String>,
    version: Value,
    url: String,
    url_specs: Value,
    sha256: Value,
    auto_updates: Option<bool>,
    disabled: bool,
    disable_reason: Option<String>,
    container: Value,
    depends_on: Value,
    conflicts_with: Value,
    artifacts: Vec<Value>,
    rename: Vec<Value>,
    supported_platforms: Vec<String>,
    variations: Value,
}

fn merge_variation(base: Value, tag: &str) -> Result<RawCask, CaskPlanError> {
    let mut base_obj = match base {
        Value::Object(map) => map,
        _ => return Err(CaskPlanError::NotAnObject),
    };
    let overlay = base_obj
        .get("variations")
        .and_then(Value::as_object)
        .and_then(|vars| vars.get(tag))
        .and_then(Value::as_object)
        .cloned();
    if let Some(overlay) = overlay {
        for (key, value) in overlay {
            base_obj.insert(key, value);
        }
    }
    serde_json::from_value(Value::Object(base_obj)).map_err(CaskPlanError::ParseMetadata)
}

pub(super) fn plan_from_value(
    payload: Value,
    requested_token: &str,
    tag: &str,
) -> Result<PlannedCask, CaskPlanError> {
    let raw = merge_variation(payload, tag)?;
    if raw.token.is_empty() {
        return Err(CaskPlanError::MissingToken {
            requested: requested_token.to_owned(),
        });
    }
    let token = validate_cask_identifier(&raw.token, "cask token")?.to_string();
    let tap = if raw.tap.trim().is_empty() {
        CASK_TAP
    } else {
        raw.tap.trim()
    };
    if tap != CASK_TAP {
        return Err(CaskPlanError::UnsupportedTap {
            token,
            tap: tap.to_owned(),
        });
    }
    if raw.disabled {
        let reason = raw
            .disable_reason
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or("disabled upstream");
        return Err(CaskPlanError::Disabled {
            token,
            reason: reason.to_owned(),
        });
    }

    let version = parse_cask_version(&raw.version, &token)?;
    let mut install_blockers = Vec::new();

    if !is_supported_macos_tag(tag) {
        install_blockers.push("native cask installation requires a supported macOS tag".into());
    }
    if !raw.supported_platforms.is_empty() && !raw.supported_platforms.iter().any(|p| p == tag) {
        install_blockers.push(format!("does not support host platform '{tag}'"));
    }

    let sha256 = validate_cask_sha256(&raw.sha256, &mut install_blockers);
    let url = validate_plan_url(&raw.url, &mut install_blockers);

    if matches!(&raw.url_specs, Value::Object(map) if !map.is_empty())
        || (!raw.url_specs.is_null() && !raw.url_specs.is_object())
    {
        install_blockers
            .push("declares custom url_specs not supported by this native installer".into());
    }
    if !raw.rename.is_empty() {
        install_blockers
            .push("declares rename steps not supported by this native installer".into());
    }

    let container = review_container(&raw.container, &mut install_blockers);
    review_depends_on(&raw.depends_on, tag, &mut install_blockers);
    let (conflicts_with_casks, conflicts_with_formulae) =
        review_conflicts_with(&token, &raw.conflicts_with, &mut install_blockers);
    let artifacts = review_artifacts(&raw.artifacts, &mut install_blockers);

    Ok(PlannedCask {
        token,
        name: raw.name,
        version,
        auto_updates: raw.auto_updates == Some(true),
        url,
        sha256,
        container,
        artifacts,
        conflicts_with_casks,
        conflicts_with_formulae,
        install_blockers,
    })
}

fn parse_cask_version(value: &Value, token: &str) -> Result<String, CaskPlanError> {
    let raw = match value {
        Value::String(s) => s.trim(),
        _ => {
            return Err(CaskPlanError::VersionNotString {
                token: token.to_owned(),
            });
        }
    };
    if raw.is_empty() || raw.len() > MAX_VERSION_LEN {
        return Err(CaskPlanError::VersionLength {
            token: token.to_owned(),
        });
    }
    if raw == "."
        || raw == ".."
        || raw.contains("..")
        || raw.contains('/')
        || raw.contains('\\')
        || raw.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(CaskPlanError::UnsafeVersion {
            token: token.to_owned(),
            raw: raw.to_owned(),
        });
    }
    Ok(raw.to_string())
}

fn validate_cask_sha256(value: &Value, blockers: &mut Vec<String>) -> String {
    match value {
        Value::String(s) if s == "no_check" => {
            blockers.push("does not publish a SHA-256 checksum (sha256: no_check)".into());
            String::new()
        }
        Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            if is_lower_hex_digest(&lower) {
                lower
            } else {
                blockers.push("does not declare a valid 64-character SHA-256 checksum".into());
                String::new()
            }
        }
        _ => {
            blockers.push("does not declare a valid 64-character SHA-256 checksum".into());
            String::new()
        }
    }
}

fn validate_plan_url(raw: &str, blockers: &mut Vec<String>) -> String {
    match validate_cask_url(raw) {
        Ok(url) => url.to_string(),
        Err(error) => {
            blockers.push(format!("declares an unsupported download URL: {error}"));
            String::new()
        }
    }
}

fn review_container(value: &Value, blockers: &mut Vec<String>) -> ContainerHint {
    match value {
        Value::Null => ContainerHint::Auto,
        Value::Object(map) if map.is_empty() => ContainerHint::Auto,
        Value::Object(map) => {
            if map.len() == 1
                && let Some(kind) = map.get("type").and_then(Value::as_str)
            {
                return match kind {
                    "naked" => ContainerHint::Naked,
                    "zip" => ContainerHint::Zip,
                    "dmg" => ContainerHint::Dmg,
                    other => {
                        blockers.push(format!("declares an unsupported container type '{other}'"));
                        ContainerHint::Auto
                    }
                };
            }
            blockers.push("declares an unsupported container configuration".into());
            ContainerHint::Auto
        }
        _ => {
            blockers.push("declares an unsupported container configuration".into());
            ContainerHint::Auto
        }
    }
}
