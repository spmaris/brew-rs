//! Parsing of individual cask artifact declarations into planned artifacts.

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::model::{CaskSourcePath, PlannedCaskArtifact};

pub(super) fn parse_app_artifact(
    app_val: &Value,
    obj: &Map<String, Value>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let (source, inner_target) = parse_two_element_artifact(app_val, "app", blockers)?;
    if !is_safe_relative_payload_path(&source) || !source.ends_with(".app") {
        blockers.push("declares an invalid app source path".into());
        return None;
    }
    let default_name = source.rsplit('/').next().unwrap_or(&source);
    let inner_bundle = match inner_target {
        Some(t) => {
            if !is_safe_single_component(&t) || !t.ends_with(".app") {
                blockers.push("declares an unsupported app target name".into());
                return None;
            }
            Some(t)
        }
        None => None,
    };

    let bundle_name = match obj.get("target") {
        Some(Value::String(target)) => {
            let Some(name) = target.strip_prefix("/Applications/") else {
                blockers.push(format!(
                    "declares app target '{target}' outside /Applications"
                ));
                return None;
            };
            if !is_safe_single_component(name) || !name.ends_with(".app") {
                blockers.push("declares an unsupported app target path".into());
                return None;
            }
            if let Some(ref inner) = inner_bundle
                && inner != name
            {
                blockers.push("declares conflicting app target names".into());
                return None;
            }
            name.to_string()
        }
        Some(_) => {
            blockers.push("declares malformed app target metadata".into());
            return None;
        }
        None => inner_bundle.unwrap_or_else(|| default_name.to_string()),
    };

    Some(PlannedCaskArtifact::App {
        source,
        bundle_name,
    })
}

pub(super) fn parse_binary_artifact(
    val: &Value,
    obj: &Map<String, Value>,
    declared_bundles: &HashSet<String>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let (source_raw, _inner_target) = parse_two_element_artifact(val, "binary", blockers)?;
    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        blockers.push("declares a binary artifact without a target path".into());
        return None;
    };
    let Some(link_name) = target.strip_prefix("$HOMEBREW_PREFIX/bin/") else {
        blockers.push(format!(
            "declares binary target '{target}' outside $HOMEBREW_PREFIX/bin"
        ));
        return None;
    };
    if !is_safe_single_component(link_name) || link_name.contains('$') {
        blockers.push(format!("declares an unsupported binary target '{target}'"));
        return None;
    }
    let source = parse_cask_source_path(&source_raw, declared_bundles, "binary", blockers)?;
    Some(PlannedCaskArtifact::Binary {
        source,
        link_name: link_name.to_string(),
    })
}

pub(super) fn parse_command_wrapper_artifact(
    val: &Value,
    obj: &Map<String, Value>,
    declared_bundles: &HashSet<String>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let Some(arr) = val.as_array() else {
        blockers.push("declares malformed command_wrapper metadata".into());
        return None;
    };
    if arr.len() != 2 {
        blockers.push("declares an unsupported command_wrapper specification".into());
        return None;
    }
    let Some(wrapper_name) = arr[0].as_str() else {
        blockers.push("declares malformed command_wrapper name".into());
        return None;
    };
    let Some(opts) = arr[1].as_object() else {
        blockers.push("declares an unsupported command_wrapper specification".into());
        return None;
    };
    for key in opts.keys() {
        if !matches!(key.as_str(), "executable" | "args") {
            blockers.push(format!(
                "declares command_wrapper option '{key}' not supported by this native installer"
            ));
            return None;
        }
    }
    let Some(exec_raw) = opts.get("executable").and_then(Value::as_str) else {
        blockers.push("declares command_wrapper without an executable".into());
        return None;
    };
    let executable =
        parse_cask_source_path(exec_raw, declared_bundles, "command_wrapper", blockers)?;

    let mut args = Vec::new();
    if let Some(raw_args) = opts.get("args") {
        let Some(items) = raw_args.as_array() else {
            blockers.push("declares malformed command_wrapper args".into());
            return None;
        };
        if items.len() > 32 {
            blockers.push("declares too many command_wrapper arguments".into());
            return None;
        }
        for item in items {
            let Some(arg) = item.as_str() else {
                blockers.push("declares non-string command_wrapper argument".into());
                return None;
            };
            if arg.contains('$') || arg.chars().any(|ch| ch.is_control()) {
                blockers.push("declares unsupported command_wrapper argument".into());
                return None;
            }
            args.push(arg.to_string());
        }
    }

    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        blockers.push("declares command_wrapper without a target path".into());
        return None;
    };
    let Some(link_name) = target.strip_prefix("$HOMEBREW_PREFIX/bin/") else {
        blockers.push(format!(
            "declares command_wrapper target '{target}' outside $HOMEBREW_PREFIX/bin"
        ));
        return None;
    };
    if !is_safe_single_component(link_name)
        || link_name.contains('$')
        || !is_safe_single_component(wrapper_name)
    {
        blockers.push(format!(
            "declares an unsupported command_wrapper target '{target}'"
        ));
        return None;
    }

    Some(PlannedCaskArtifact::CommandWrapper {
        executable,
        link_name: link_name.to_string(),
        args,
    })
}

pub(super) fn parse_prefix_link_artifact(
    kind: &str,
    val: &Value,
    obj: &Map<String, Value>,
    declared_bundles: &HashSet<String>,
    blockers: &mut Vec<String>,
) -> Option<PlannedCaskArtifact> {
    let (source_raw, _inner_target) = parse_two_element_artifact(val, kind, blockers)?;
    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        blockers.push(format!("declares {kind} artifact without a target path"));
        return None;
    };
    let Some(rel) = target.strip_prefix("$HOMEBREW_PREFIX/") else {
        blockers.push(format!(
            "declares {kind} target '{target}' outside $HOMEBREW_PREFIX"
        ));
        return None;
    };
    let valid_dest = match kind {
        "manpage" => rel.strip_prefix("share/man/").is_some_and(|rest| {
            let Some((section, file)) = rest.split_once('/') else {
                return false;
            };
            matches!(
                section,
                "man1" | "man2" | "man3" | "man4" | "man5" | "man6" | "man7" | "man8"
            ) && is_safe_single_component(file)
                && !file.contains('$')
        }),
        "bash_completion" => rel
            .strip_prefix("etc/bash_completion.d/")
            .is_some_and(|file| is_safe_single_component(file) && !file.contains('$')),
        "zsh_completion" => rel
            .strip_prefix("share/zsh/site-functions/")
            .is_some_and(|file| is_safe_single_component(file) && !file.contains('$')),
        "fish_completion" => rel
            .strip_prefix("share/fish/vendor_completions.d/")
            .is_some_and(|file| is_safe_single_component(file) && !file.contains('$')),
        _ => false,
    };
    if !valid_dest {
        blockers.push(format!("declares an unsupported {kind} target '{target}'"));
        return None;
    }

    let source = parse_cask_source_path(&source_raw, declared_bundles, kind, blockers)?;
    Some(PlannedCaskArtifact::PrefixLink {
        source,
        relative_dest: rel.to_string(),
    })
}

fn parse_two_element_artifact(
    val: &Value,
    kind: &str,
    blockers: &mut Vec<String>,
) -> Option<(String, Option<String>)> {
    let Some(arr) = val.as_array() else {
        blockers.push(format!("declares malformed {kind} artifact metadata"));
        return None;
    };
    match arr.as_slice() {
        [Value::String(source)] => Some((source.clone(), None)),
        [Value::String(source), Value::Object(opts)] => {
            if opts.len() != 1 || !opts.contains_key("target") {
                blockers.push(format!(
                    "declares unsupported {kind} options not supported by this native installer"
                ));
                return None;
            }
            let Some(target) = opts.get("target").and_then(Value::as_str) else {
                blockers.push(format!("declares malformed {kind} target option"));
                return None;
            };
            Some((source.clone(), Some(target.to_string())))
        }
        _ => {
            blockers.push(format!("declares unsupported {kind} artifact array shape"));
            None
        }
    }
}

fn parse_cask_source_path(
    source: &str,
    declared_bundles: &HashSet<String>,
    kind: &str,
    blockers: &mut Vec<String>,
) -> Option<CaskSourcePath> {
    if let Some(rest) = source.strip_prefix("$APPDIR/") {
        let Some((bundle, inner)) = rest.split_once('/') else {
            blockers.push(format!(
                "declares invalid $APPDIR source path in {kind} artifact"
            ));
            return None;
        };
        if !is_safe_single_component(bundle)
            || !bundle.ends_with(".app")
            || !declared_bundles.contains(bundle)
            || !is_safe_relative_payload_path(inner)
            || inner.contains('$')
        {
            blockers.push(format!(
                "declares unsupported $APPDIR source '{source}' in {kind} artifact"
            ));
            return None;
        }
        return Some(CaskSourcePath::AppBundle {
            bundle_name: bundle.to_string(),
            inner_path: inner.to_string(),
        });
    }
    if source.starts_with('/') || source.contains('$') || !is_safe_relative_payload_path(source) {
        blockers.push(format!(
            "declares unsupported source path '{source}' in {kind} artifact"
        ));
        return None;
    }
    Some(CaskSourcePath::Payload(source.to_string()))
}

fn is_safe_single_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.chars().any(|ch| ch.is_control())
}

fn is_safe_relative_payload_path(path: &str) -> bool {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.chars().any(|ch| ch.is_control())
    {
        return false;
    }
    path.split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}
