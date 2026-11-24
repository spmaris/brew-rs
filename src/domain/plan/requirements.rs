//! Review of formula `requirements` entries against the host bottle tag.

use serde_json::Value;

use super::macos::{arch_for_tag, macos_version_for_tag, parse_macos_version};
use super::validate::validate_formula_name;

/// Maximum install requirements read from a single formula.
const MAX_REQUIREMENTS_PER_FORMULA: usize = 8;
/// Maximum contexts an accepted macOS requirement may list (only none today).
const MAX_REQUIREMENT_CONTEXTS: usize = 8;
/// Maximum specs an accepted macOS requirement may list.
const MAX_REQUIREMENT_SPECS: usize = 2;

/// Blocker for install requirements outside the modelled macOS minimum shape.
pub(super) const BLOCKER_REQUIREMENTS_SHAPE: &str =
    "declares additional install requirements not supported by this native installer";

/// Blocker for macOS requirements declared on a non-macOS host.
pub(super) const BLOCKER_REQUIREMENTS_PLATFORM: &str =
    "declares macOS install requirements on a host platform macOS does not serve";

/// Blocker for a macOS minimum newer than the host bottle tag's release.
pub(super) const BLOCKER_REQUIREMENTS_VERSION: &str =
    "requires a newer macOS release than the host bottle tag";

/// Blocker for an architecture requirement that disagrees with the host bottle tag.
pub(super) const BLOCKER_REQUIREMENTS_ARCH: &str =
    "requires a different CPU architecture than the host bottle tag";

/// Exact keys of a Homebrew formula `requirements` entry.
const MACOS_REQUIREMENT_KEYS: [&str; 6] =
    ["name", "cask", "download", "version", "contexts", "specs"];

/// Review a formula's `requirements` array against host tag `tag`.
pub(super) fn review_requirements(requirements: &[Value], tag: &str) -> Option<&'static str> {
    if requirements.is_empty() {
        return None;
    }
    if requirements.len() > MAX_REQUIREMENTS_PER_FORMULA {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    for requirement in requirements {
        if let Some(blocker) = review_requirement(requirement, tag) {
            return Some(blocker);
        }
    }
    None
}

/// Review one requirement entry against host tag `tag`.
fn review_requirement(requirement: &Value, tag: &str) -> Option<&'static str> {
    let Some(requirement) = requirement.as_object() else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if requirement.len() != MACOS_REQUIREMENT_KEYS.len()
        || !MACOS_REQUIREMENT_KEYS
            .iter()
            .all(|key| requirement.contains_key(*key))
    {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    let Some(name) = requirement.get("name").and_then(Value::as_str) else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if !requirement.get("cask").is_some_and(Value::is_null)
        || !requirement.get("download").is_some_and(Value::is_null)
    {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }

    let Some(specs) = requirement.get("specs").and_then(Value::as_array) else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if specs.len() > MAX_REQUIREMENT_SPECS {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    let specs_are_stable = match specs.as_slice() {
        [stable] => stable.as_str() == Some("stable"),
        [stable, head] => stable.as_str() == Some("stable") && head.as_str() == Some("head"),
        _ => false,
    };
    if !specs_are_stable {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }

    let Some(contexts) = requirement.get("contexts").and_then(Value::as_array) else {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    };
    if contexts.len() > MAX_REQUIREMENT_CONTEXTS {
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }
    if !contexts.is_empty() {
        let build_or_test_only = contexts.len() <= 2
            && contexts
                .iter()
                .all(|context| matches!(context.as_str(), Some("build" | "test")));
        if build_or_test_only && validate_formula_name(name, "requirement name").is_ok() {
            return None;
        }
        return Some(BLOCKER_REQUIREMENTS_SHAPE);
    }

    match name {
        "macos" => {
            let required = match requirement.get("version") {
                Some(Value::Null) => None,
                Some(Value::String(version)) => {
                    let Some(parsed) = parse_macos_version(version) else {
                        return Some(BLOCKER_REQUIREMENTS_SHAPE);
                    };
                    Some(parsed)
                }
                _ => return Some(BLOCKER_REQUIREMENTS_SHAPE),
            };
            let Some(host) = macos_version_for_tag(tag) else {
                return Some(BLOCKER_REQUIREMENTS_PLATFORM);
            };
            if let Some(required) = required
                && host < required
            {
                return Some(BLOCKER_REQUIREMENTS_VERSION);
            }
            None
        }
        "arch" => {
            let Some(required_arch) = requirement.get("version").and_then(Value::as_str) else {
                return Some(BLOCKER_REQUIREMENTS_SHAPE);
            };
            if !matches!(required_arch, "arm64" | "x86_64") {
                return Some(BLOCKER_REQUIREMENTS_SHAPE);
            }
            let Some(host_arch) = arch_for_tag(tag) else {
                return Some(BLOCKER_REQUIREMENTS_PLATFORM);
            };
            if host_arch != required_arch {
                return Some(BLOCKER_REQUIREMENTS_ARCH);
            }
            None
        }
        _ => Some(BLOCKER_REQUIREMENTS_SHAPE),
    }
}
