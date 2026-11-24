//! Validation of one API payload into a graph node, with its install blockers.

use std::collections::HashSet;

use super::error::MetadataError;
use super::macos::is_supported_macos_tag;
use super::model::{FormulaNode, FormulaResponse};
use super::requirements::review_requirements;
use super::service::review_service;
use super::uses_from_macos::review_uses_from_macos;
use super::validate::{
    CORE_TAP, available_tags, validate_bottle_url, validate_cellar, validate_formula_name,
    validate_sha256, validate_version,
};

/// Maximum runtime dependencies read from a single formula.
pub(super) const MAX_DEPS_PER_FORMULA: usize = 256;

/// Blocker for default-installed recommended dependencies.
pub(super) const BLOCKER_RECOMMENDED_DEPENDENCY: &str =
    "declares recommended dependencies this native installer does not install";

/// Blocker for a pour-bottle-only condition.
pub(super) const BLOCKER_POUR_BOTTLE_ONLY: &str =
    "declares pour-bottle-only conditions not supported by this native installer";

/// Build and validate one graph node from an API payload.
pub(super) fn build_node(
    response: FormulaResponse,
    tag: &str,
) -> Result<(String, FormulaNode), MetadataError> {
    let name = validate_formula_name(&response.name, "formula name")?.to_string();

    if response.tap != CORE_TAP {
        return Err(MetadataError::UnsupportedTap {
            tap: response.tap,
            name,
        });
    }
    if response.disabled {
        return Err(MetadataError::Disabled { name });
    }

    if !response.versions.bottle {
        return Err(MetadataError::NoBottledStable { name });
    }
    let stable = response
        .versions
        .stable
        .as_deref()
        .ok_or_else(|| MetadataError::NoStableVersion { name: name.clone() })?;
    if stable.is_empty() {
        return Err(MetadataError::EmptyStableVersion { name });
    }

    let bottle = response
        .bottle
        .as_ref()
        .and_then(|bottle| bottle.stable.as_ref())
        .ok_or_else(|| MetadataError::NoStableBottle { name: name.clone() })?;
    // `all` is Homebrew's platform-independent bottle.
    let bottle_file = bottle
        .files
        .get(tag)
        .or_else(|| bottle.files.get("all"))
        .ok_or_else(|| MetadataError::NoBottleForTag {
            name: name.clone(),
            tag: tag.to_owned(),
            available: available_tags(&bottle.files),
        })?;

    let revision = response.revision.unwrap_or(0);
    let version = if revision == 0 {
        stable.to_string()
    } else {
        format!("{stable}_{revision}")
    };
    validate_version(&version, &name)?;

    let sha256 = validate_sha256(&bottle_file.sha256, &name)?;
    let url = validate_bottle_url(&bottle_file.url, &sha256, &name)?;
    let cellar = validate_cellar(&bottle_file.cellar, &name)?;

    let dependency_list = response
        .variations
        .get(tag)
        .and_then(|variation| variation.dependencies.as_ref())
        .unwrap_or(&response.dependencies);
    if dependency_list.len() > MAX_DEPS_PER_FORMULA {
        return Err(MetadataError::TooManyDependencies {
            name,
            count: dependency_list.len(),
        });
    }

    let mut dependencies = Vec::with_capacity(dependency_list.len());
    let mut seen = HashSet::new();
    for dependency in dependency_list {
        let dependency = validate_formula_name(dependency, "dependency name")?.to_string();
        if seen.insert(dependency.clone()) {
            dependencies.push(dependency);
        }
    }

    // Portable bottles and the validated macOS relocation lane can be staged.
    // Keep other fixed-prefix and relocation formats available to plan/fetch,
    // but ineligible for installation.
    let mut install_blockers = Vec::new();
    if response.version_scheme.is_none() {
        install_blockers.push("does not declare a Homebrew version scheme");
    }
    match cellar.as_str() {
        ":any_skip_relocation" => {}
        ":any" if is_supported_macos_tag(tag) => {}
        ":any" => install_blockers
            .push("requires bottle relocation not yet supported for this formula or platform"),
        _ => install_blockers.push("requires a fixed Homebrew prefix"),
    }
    if !response.recommended_dependencies.is_empty() {
        install_blockers.push(BLOCKER_RECOMMENDED_DEPENDENCY);
    }
    if let Some(blocker) = review_uses_from_macos(&response, tag) {
        install_blockers.push(blocker);
    }
    if response.post_install_defined != Some(false) {
        install_blockers.push("has a post-install hook or does not declare hook status");
    }
    let keg_only = match response.keg_only {
        Some(flag) => flag,
        None => {
            install_blockers.push("does not declare keg-only status");
            false
        }
    };
    let conflicts_with = validate_conflicts(&name, &response.conflicts_with, &mut install_blockers);
    let has_service = !response.service.is_null();
    if let Some(blocker) = review_service(&response.service, tag) {
        install_blockers.push(blocker);
    }
    if !response.pour_bottle_only_if.is_null() {
        install_blockers.push(BLOCKER_POUR_BOTTLE_ONLY);
    }
    if let Some(blocker) = review_requirements(&response.requirements, tag) {
        install_blockers.push(blocker);
    }
    if response
        .variations
        .get(tag)
        .is_some_and(|variation| !variation.extra.is_empty())
    {
        install_blockers
            .push("declares platform-specific metadata not understood by the installer");
    }

    Ok((
        name.clone(),
        FormulaNode {
            name,
            version,
            version_scheme: response.version_scheme,
            keg_only,
            has_service,
            caveats: response.caveats.clone(),
            conflicts_with,
            dependencies,
            url,
            sha256,
            cellar,
            install_blockers,
        },
    ))
}

fn validate_conflicts(
    formula_name: &str,
    raw: &[String],
    install_blockers: &mut Vec<&'static str>,
) -> Vec<String> {
    if raw.len() > MAX_DEPS_PER_FORMULA {
        install_blockers.push("declares an unsupported conflict rule");
        return Vec::new();
    }
    let mut validated = Vec::with_capacity(raw.len());
    let mut seen = HashSet::new();
    for conflict in raw {
        let Ok(name) = validate_formula_name(conflict, "conflict formula name") else {
            install_blockers.push("declares an unsupported conflict rule");
            return Vec::new();
        };
        if name == formula_name {
            install_blockers.push("declares an unsupported conflict rule");
            return Vec::new();
        }
        if seen.insert(name) {
            validated.push(name.to_string());
        }
    }
    validated.sort_unstable();
    validated
}
