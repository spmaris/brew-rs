//! Review of `uses_from_macos` declarations against the host bottle tag.

use serde_json::Value;

use super::macos::{MACOS_RELEASES, MacosVersion, macos_version_for_tag};
use super::model::FormulaResponse;
use super::validate::validate_formula_name;

/// `uses_from_macos` libraries verified against macOS system libraries linked by
/// official Homebrew bottles or macOS-supplied system tools/runtimes.
pub(super) const SYSTEM_SUPPLIED_LIBRARIES: [&str; 14] = [
    "bzip2", "cups", "curl", "expat", "krb5", "libedit", "libffi", "libiconv", "libxml2",
    "libxslt", "ncurses", "openldap", "python", "zlib",
];

/// Blocker for a `uses_from_macos` declaration whose JSON shape is not modelled.
pub(super) const BLOCKER_USES_FROM_MACOS_SHAPE: &str =
    "declares a uses_from_macos metadata shape not supported by this native installer";

/// Blocker for a `uses_from_macos` library that is not a verified macOS one.
pub(super) const BLOCKER_USES_FROM_MACOS_DEPENDENCY: &str =
    "declares a uses_from_macos dependency that is not a verified macOS system library";

/// Blocker for tagged or version-bounded `uses_from_macos` declarations.
pub(super) const BLOCKER_USES_FROM_MACOS_SCOPING: &str = "declares conditional or build-scoped uses_from_macos dependencies not supported by this native installer";

/// Blocker for `uses_from_macos` on a platform where macOS cannot supply it.
pub(super) const BLOCKER_USES_FROM_MACOS_PLATFORM: &str =
    "declares uses_from_macos dependencies on a platform where macOS does not supply them";

/// Review a formula's `uses_from_macos` metadata for host tag `tag`.
///
/// Accepts plain untagged entries from [`SYSTEM_SUPPLIED_LIBRARIES`] when their
/// bounds entry is `{}` or `{ "since": "<known_codename>" }` with host release
/// `>=` that release, plus build/test-only entries validated by
/// [`is_build_or_test_scoped_uses_from_macos`]. On non-macOS hosts any non-empty
/// declaration keeps [`BLOCKER_USES_FROM_MACOS_PLATFORM`].
pub(super) fn review_uses_from_macos(
    response: &FormulaResponse,
    tag: &str,
) -> Option<&'static str> {
    let declarations = &response.uses_from_macos;
    let bounds = &response.uses_from_macos_bounds;

    // The official API serialises exactly one bounds entry per declaration. A
    // payload whose arrays disagree, or that records bounds without
    // declarations, cannot be interpreted safely.
    if declarations.len() != bounds.len() {
        return Some(BLOCKER_USES_FROM_MACOS_SHAPE);
    }
    if declarations.is_empty() {
        return None;
    }

    let Some(host) = macos_version_for_tag(tag) else {
        return Some(BLOCKER_USES_FROM_MACOS_PLATFORM);
    };

    for (declaration, bounds) in declarations.iter().zip(bounds) {
        match declaration {
            serde_json::Value::String(name) => {
                if !SYSTEM_SUPPLIED_LIBRARIES.contains(&name.as_str()) {
                    return Some(BLOCKER_USES_FROM_MACOS_DEPENDENCY);
                }
                if let Some(blocker) = review_system_library_bounds(bounds, host) {
                    return Some(blocker);
                }
            }
            serde_json::Value::Object(map) => {
                if is_build_or_test_scoped_uses_from_macos(map, bounds) {
                    continue;
                }
                if is_no_linkage_uses_from_macos(map) {
                    let name = map.keys().next().expect("map.len() == 1");
                    if !SYSTEM_SUPPLIED_LIBRARIES.contains(&name.as_str()) {
                        return Some(BLOCKER_USES_FROM_MACOS_DEPENDENCY);
                    }
                    if let Some(blocker) = review_system_library_bounds(bounds, host) {
                        return Some(blocker);
                    }
                    continue;
                }
                let is_recognized_tagged_object = map.len() == 1
                    && map.iter().all(|(key, value)| {
                        validate_formula_name(key, "uses_from_macos dependency").is_ok()
                            && (value.is_string() || value.is_array())
                    });
                return Some(if is_recognized_tagged_object {
                    BLOCKER_USES_FROM_MACOS_SCOPING
                } else {
                    BLOCKER_USES_FROM_MACOS_SHAPE
                });
            }
            _ => return Some(BLOCKER_USES_FROM_MACOS_SHAPE),
        }
    }

    None
}

fn review_system_library_bounds(bounds: &Value, host: MacosVersion) -> Option<&'static str> {
    let Some(bounds) = bounds.as_object() else {
        return Some(BLOCKER_USES_FROM_MACOS_SHAPE);
    };
    if bounds.is_empty() {
        return None;
    }
    let satisfied = bounds.len() == 1
        && bounds
            .get("since")
            .and_then(Value::as_str)
            .and_then(macos_version_for_tag)
            .is_some_and(|since| host >= since);
    if satisfied {
        None
    } else {
        Some(BLOCKER_USES_FROM_MACOS_SCOPING)
    }
}

fn is_no_linkage_uses_from_macos(map: &serde_json::Map<String, Value>) -> bool {
    if map.len() != 1 {
        return false;
    }
    let Some((name, scope)) = map.iter().next() else {
        return false;
    };
    if validate_formula_name(name, "uses_from_macos dependency").is_err() {
        return false;
    }
    match scope {
        Value::String(tag) => tag == "no_linkage",
        Value::Array(tags) => tags.len() == 1 && tags[0].as_str() == Some("no_linkage"),
        _ => false,
    }
}

fn is_build_or_test_scoped_uses_from_macos(
    map: &serde_json::Map<String, Value>,
    bounds: &Value,
) -> bool {
    if map.len() != 1 {
        return false;
    }
    let Some((name, scope)) = map.iter().next() else {
        return false;
    };
    if validate_formula_name(name, "uses_from_macos dependency").is_err() {
        return false;
    }
    let is_build_or_test_tag = |tag: &Value| matches!(tag.as_str(), Some("build" | "test"));
    let scope_ok = match scope {
        Value::String(_) => is_build_or_test_tag(scope),
        Value::Array(tags) => {
            !tags.is_empty() && tags.len() <= 2 && tags.iter().all(is_build_or_test_tag)
        }
        _ => false,
    };
    if !scope_ok {
        return false;
    }
    let Some(bounds) = bounds.as_object() else {
        return false;
    };
    bounds.iter().all(|(key, value)| {
        matches!(key.as_str(), "since" | "before")
            && value.as_str().is_some_and(|codename| {
                MACOS_RELEASES
                    .iter()
                    .any(|(known, _, _)| *known == codename)
            })
    })
}
