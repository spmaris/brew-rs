//! Review of the artifact list of a cask into planned artifacts.

use std::collections::HashSet;

use serde_json::Value;

use super::artifact_parse::{
    parse_app_artifact, parse_binary_artifact, parse_command_wrapper_artifact,
    parse_prefix_link_artifact,
};
use super::model::PlannedCaskArtifact;

/// Maximum number of artifacts declared by a single cask.
const MAX_ARTIFACTS_PER_CASK: usize = 128;

pub(super) fn review_artifacts(
    raw_artifacts: &[Value],
    blockers: &mut Vec<String>,
) -> Vec<PlannedCaskArtifact> {
    if raw_artifacts.len() > MAX_ARTIFACTS_PER_CASK {
        blockers.push(format!(
            "declares more than {MAX_ARTIFACTS_PER_CASK} artifacts"
        ));
        return Vec::new();
    }

    // Pass 1: validate and collect `app` bundles so pass 2 can verify any
    // `$APPDIR/<Bundle>.app/...` source paths belong to this cask.
    let mut apps = Vec::new();
    let mut declared_bundles = HashSet::new();
    for entry in raw_artifacts {
        let Some(obj) = entry.as_object() else {
            blockers.push("declares malformed artifact metadata".into());
            return Vec::new();
        };
        if let Some(app_val) = obj.get("app")
            && let Some(artifact) = parse_app_artifact(app_val, obj, blockers)
        {
            if let PlannedCaskArtifact::App {
                ref bundle_name, ..
            } = artifact
                && !declared_bundles.insert(bundle_name.clone())
            {
                blockers.push(format!("declares duplicate app bundle '{bundle_name}'"));
                return Vec::new();
            }
            apps.push(artifact);
        }
    }

    // Pass 2: validate all remaining artifact declarations.
    let mut artifacts = apps;
    let mut seen_prefix_dests = HashSet::new();

    for entry in raw_artifacts {
        let Some(obj) = entry.as_object() else {
            return Vec::new();
        };
        let keys: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "target")
            .collect();
        if keys.len() != 1 {
            blockers.push("declares an unsupported multi-key artifact entry".into());
            return Vec::new();
        }
        let kind = keys[0];
        let value = &obj[kind];
        match kind {
            "app" | "zap" => {}
            "uninstall" => review_uninstall_artifact(value, blockers),
            "binary" => {
                if let Some(artifact) =
                    parse_binary_artifact(value, obj, &declared_bundles, blockers)
                {
                    if let PlannedCaskArtifact::Binary { ref link_name, .. } = artifact {
                        let dest = format!("bin/{link_name}");
                        if !seen_prefix_dests.insert(dest.clone()) {
                            blockers.push(format!("declares duplicate destination '{dest}'"));
                            return Vec::new();
                        }
                    }
                    artifacts.push(artifact);
                }
            }
            "command_wrapper" => {
                if let Some(artifact) =
                    parse_command_wrapper_artifact(value, obj, &declared_bundles, blockers)
                {
                    if let PlannedCaskArtifact::CommandWrapper { ref link_name, .. } = artifact {
                        let dest = format!("bin/{link_name}");
                        if !seen_prefix_dests.insert(dest.clone()) {
                            blockers.push(format!("declares duplicate destination '{dest}'"));
                            return Vec::new();
                        }
                    }
                    artifacts.push(artifact);
                }
            }
            "manpage" | "bash_completion" | "zsh_completion" | "fish_completion" => {
                if let Some(artifact) =
                    parse_prefix_link_artifact(kind, value, obj, &declared_bundles, blockers)
                {
                    if let PlannedCaskArtifact::PrefixLink {
                        ref relative_dest, ..
                    } = artifact
                        && !seen_prefix_dests.insert(relative_dest.clone())
                    {
                        blockers.push(format!("declares duplicate destination '{relative_dest}'"));
                        return Vec::new();
                    }
                    artifacts.push(artifact);
                }
            }
            other => {
                blockers.push(format!(
                    "declares unsupported '{other}' artifact not supported by this native installer"
                ));
            }
        }
    }

    let has_primary = artifacts.iter().any(|artifact| {
        matches!(
            artifact,
            PlannedCaskArtifact::App { .. }
                | PlannedCaskArtifact::Binary { .. }
                | PlannedCaskArtifact::CommandWrapper { .. }
        )
    });
    if !has_primary && blockers.is_empty() {
        blockers
            .push("does not declare any supported app, binary, or command_wrapper artifact".into());
    }
    artifacts
}

fn review_uninstall_artifact(value: &Value, blockers: &mut Vec<String>) {
    let Some(items) = value.as_array() else {
        blockers.push("declares uninstall steps not supported by this native installer".into());
        return;
    };
    for item in items {
        let Some(map) = item.as_object() else {
            blockers.push("declares uninstall steps not supported by this native installer".into());
            return;
        };
        let supported = map
            .keys()
            .all(|k| matches!(k.as_str(), "quit" | "signal" | "login_item" | "launchctl"));
        if !supported {
            blockers.push("declares uninstall steps not supported by this native installer".into());
            return;
        }
    }
}
