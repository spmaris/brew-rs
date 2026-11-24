use std::path::Path;

use anyhow::{Context, Result};

use crate::commands::install::prefix_path;
use crate::config::Config;
use crate::infra::store::{Store, validate_dependencies_with_retained};
use crate::ui::DnfStyleFormatter;

use super::colima::COLIMA_VERSION;
use super::observe::{ObservedEntry, SERVICE_STATE_ROOT, SERVICE_TRIAL_ROOT, observe_state};

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
