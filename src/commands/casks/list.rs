use std::path::Path;

use anyhow::Result;

use crate::commands::install::prefix_path;
use crate::config::Config;
use crate::infra::cask_store;
use crate::infra::cask_store::CaskReceipt;
use crate::infra::store::Store;
use crate::ui::DnfStyleFormatter;

use super::stage::summarize_receipt_targets;

pub(crate) fn list(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let casks = cask_store::installed(&store)?;
    let formatter = DnfStyleFormatter::new(config);
    if casks.is_empty() {
        formatter.note(&format!("No casks in {}.", store.prefix().display()));
        return Ok(());
    }
    if formatter.boxed() {
        render_cask_list(&formatter, &casks);
    } else {
        let rows: Vec<Vec<String>> = casks
            .iter()
            .map(|cask| {
                vec![
                    "cask".to_string(),
                    cask.token.clone(),
                    cask.version.clone(),
                    "homebrew".to_string(),
                    "requested".to_string(),
                ]
            })
            .collect();
        println!(
            "{}",
            formatter.table(
                "Packages",
                &["Type", "Name", "Version", "Source", "Reason"],
                &rows
            )
        );
    }
    Ok(())
}

/// Boxed `list`: the `Casks (N)` panel with its targets. Plain output omits
/// targets and uses the shared inventory table instead.
pub(in crate::commands) fn render_cask_list(formatter: &DnfStyleFormatter, casks: &[CaskReceipt]) {
    let rows: Vec<_> = casks
        .iter()
        .map(|cask| {
            vec![
                cask.token.clone(),
                cask.version.clone(),
                summarize_receipt_targets(cask),
            ]
        })
        .collect();
    println!(
        "{}",
        formatter.table(
            &format!("Casks ({})", rows.len()),
            &["Cask", "Version", "Targets"],
            &rows,
        )
    );
}
