//! Review of cask `depends_on` and `conflicts_with` metadata.

use std::collections::HashSet;

use serde_json::Value;

use super::super::plan::{arch_for_tag, macos_version_for_tag, parse_macos_version_or_codename};
use super::identifier::validate_cask_identifier;

pub(super) fn review_depends_on(value: &Value, tag: &str, blockers: &mut Vec<String>) {
    let map = match value {
        Value::Null => return,
        Value::Object(map) => map,
        _ => {
            blockers.push("declares malformed depends_on metadata".into());
            return;
        }
    };
    for (key, constraint) in map {
        match key.as_str() {
            "macos" => review_cask_macos_dep(constraint, tag, blockers),
            "arch" => review_cask_arch_dep(constraint, tag, blockers),
            other => blockers.push(format!(
                "declares unsupported '{other}' dependency in depends_on"
            )),
        }
    }
}

fn review_cask_macos_dep(value: &Value, tag: &str, blockers: &mut Vec<String>) {
    let Some(host) = macos_version_for_tag(tag) else {
        // Already blocked by non-macOS tag check.
        return;
    };
    let map = match value {
        Value::Null => return,
        Value::Object(map) if map.is_empty() => return,
        Value::Object(map) => map,
        _ => {
            blockers.push("declares an unsupported macOS dependency constraint".into());
            return;
        }
    };
    if map.len() != 1 {
        blockers.push("declares an unsupported macOS dependency constraint".into());
        return;
    }
    let (op, versions_val) = map.iter().next().expect("len == 1");
    let Some(versions_arr) = versions_val.as_array() else {
        blockers.push("declares an unsupported macOS dependency constraint".into());
        return;
    };
    if versions_arr.is_empty() {
        blockers.push("declares an empty macOS dependency constraint".into());
        return;
    }
    let mut parsed_versions = Vec::with_capacity(versions_arr.len());
    for entry in versions_arr {
        let Some(ver_str) = entry.as_str().and_then(parse_macos_version_or_codename) else {
            blockers.push("declares an unrecognized macOS version in depends_on".into());
            return;
        };
        parsed_versions.push(ver_str);
    }

    match op.as_str() {
        ">=" if parsed_versions.len() == 1 => {
            if host < parsed_versions[0] {
                blockers.push(format!(
                    "requires macOS >= {} (host tag '{tag}' is lower)",
                    versions_arr[0].as_str().unwrap_or_default()
                ));
            }
        }
        "==" => {
            if !parsed_versions.contains(&host) {
                blockers.push(format!("does not match host macOS release for '{tag}'"));
            }
        }
        _ => blockers.push(format!(
            "declares unsupported macOS constraint operator '{op}'"
        )),
    }
}

fn review_cask_arch_dep(value: &Value, tag: &str, blockers: &mut Vec<String>) {
    let Some(host_arch) = arch_for_tag(tag) else {
        return;
    };
    let Some(entries) = value.as_array() else {
        blockers.push("declares an unsupported arch constraint in depends_on".into());
        return;
    };
    if entries.is_empty() {
        blockers.push("declares an empty arch constraint in depends_on".into());
        return;
    }
    let mut matches_host = false;
    for entry in entries {
        let Some(obj) = entry.as_object() else {
            blockers.push("declares an unsupported arch constraint in depends_on".into());
            return;
        };
        let Some(arch_type) = obj.get("type").and_then(Value::as_str) else {
            blockers.push("declares an unsupported arch constraint in depends_on".into());
            return;
        };
        let Some(bits) = obj.get("bits").and_then(Value::as_u64) else {
            blockers.push("declares an unsupported arch constraint in depends_on".into());
            return;
        };
        if bits != 64 {
            continue;
        }
        match (arch_type, host_arch) {
            ("arm", "arm64") | ("intel", "x86_64") => matches_host = true,
            ("arm" | "intel", _) => {}
            _ => {
                blockers.push(format!(
                    "declares unrecognized arch type '{arch_type}' in depends_on"
                ));
                return;
            }
        }
    }
    if !matches_host {
        blockers.push(format!(
            "requires a different CPU architecture than '{tag}'"
        ));
    }
}

pub(super) fn review_conflicts_with(
    own_token: &str,
    value: &Value,
    blockers: &mut Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let map = match value {
        Value::Null => return (Vec::new(), Vec::new()),
        Value::Object(map) => map,
        _ => {
            blockers.push("declares an unsupported conflict rule".into());
            return (Vec::new(), Vec::new());
        }
    };
    let mut casks = Vec::new();
    let mut formulae = Vec::new();
    for (kind, entries) in map {
        let target = match kind.as_str() {
            "cask" => &mut casks,
            "formula" => &mut formulae,
            _ => {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            }
        };
        let items = match entries {
            Value::Null => continue,
            Value::String(s) => vec![Value::String(s.clone())],
            Value::Array(arr) => arr.clone(),
            _ => {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            }
        };
        let mut seen = HashSet::new();
        for item in items {
            let Some(name) = item.as_str() else {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            };
            let Ok(valid) = validate_cask_identifier(name, "conflict identifier") else {
                blockers.push("declares an unsupported conflict rule".into());
                return (Vec::new(), Vec::new());
            };
            if kind == "cask" && valid == own_token {
                blockers.push("declares a self-conflict rule".into());
                return (Vec::new(), Vec::new());
            }
            if seen.insert(valid.to_string()) {
                target.push(valid.to_string());
            }
        }
        target.sort_unstable();
    }
    (casks, formulae)
}
