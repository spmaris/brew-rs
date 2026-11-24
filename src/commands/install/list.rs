use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::infra::store::{Receipt, Store};
use crate::ui::DnfStyleFormatter;

use super::prefix_path;

pub(crate) fn list(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let receipts = store.installed()?;
    let formatter = DnfStyleFormatter::new(config);
    if receipts.is_empty() {
        formatter.note(&format!("No packages in {}.", store.prefix().display()));
        return Ok(());
    }
    render_formula_list(&formatter, receipts);
    Ok(())
}

pub(crate) fn list_all(
    config: &Config,
    prefix: Option<&Path>,
    formula_only: bool,
    cask_only: bool,
) -> Result<()> {
    if cask_only {
        return crate::commands::casks::list(config, prefix);
    }
    if formula_only {
        return list(config, prefix);
    }
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let formulae = store.installed()?;
    let casks = crate::infra::cask_store::installed(&store)?;
    let formatter = DnfStyleFormatter::new(config);
    if formulae.is_empty() && casks.is_empty() {
        formatter.note(&format!("No packages in {}.", store.prefix().display()));
        return Ok(());
    }
    if formatter.boxed() {
        let has_formulae = !formulae.is_empty();
        if has_formulae {
            render_formula_list(&formatter, formulae);
        }
        if !casks.is_empty() {
            if has_formulae {
                println!();
            }
            crate::commands::casks::render_cask_list(&formatter, &casks);
        }
    } else {
        render_inventory(&formatter, &formulae, &casks);
    }
    Ok(())
}

/// Plain `list`: formula and cask rows in one `Type Name Version Source Reason`
/// table so scripts see a single, stable schema.
pub(super) fn render_inventory(
    formatter: &DnfStyleFormatter,
    formulae: &[Receipt],
    casks: &[crate::infra::cask_store::CaskReceipt],
) {
    let mut sorted: Vec<&Receipt> = formulae.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    let mut rows: Vec<Vec<String>> = sorted
        .iter()
        .map(|receipt| {
            vec![
                "formula".to_string(),
                receipt.name.clone(),
                receipt.version.clone(),
                receipt.source.label().to_ascii_lowercase(),
                reason_token(receipt).to_string(),
            ]
        })
        .collect();
    let mut sorted_casks: Vec<&crate::infra::cask_store::CaskReceipt> = casks.iter().collect();
    sorted_casks.sort_by(|left, right| left.token.cmp(&right.token));
    for cask in sorted_casks {
        rows.push(vec![
            "cask".to_string(),
            cask.token.clone(),
            cask.version.clone(),
            "homebrew".to_string(),
            "requested".to_string(),
        ]);
    }
    println!(
        "{}",
        formatter.table(
            "Packages",
            &["Type", "Name", "Version", "Source", "Reason"],
            &rows
        )
    );
}

pub(super) fn reason_token(receipt: &Receipt) -> &'static str {
    if receipt.requested {
        "requested"
    } else {
        "dependency"
    }
}

pub(super) fn render_formula_list(formatter: &DnfStyleFormatter, mut receipts: Vec<Receipt>) {
    receipts.sort_by(|left, right| left.name.cmp(&right.name));
    let rows: Vec<_> = receipts
        .into_iter()
        .map(|receipt| {
            let reason = reason_token(&receipt).to_string();
            vec![
                receipt.name,
                receipt.version,
                receipt.source.label().to_ascii_lowercase(),
                reason,
            ]
        })
        .collect();
    let rendered = if formatter.boxed() {
        formatter.table(
            &format!("Packages ({})", rows.len()),
            &["Package", "Version", "Source", "Reason"],
            &rows,
        )
    } else {
        let rows: Vec<Vec<String>> = rows
            .into_iter()
            .map(|mut cells| {
                cells.insert(0, "formula".to_string());
                cells
            })
            .collect();
        formatter.table(
            "Packages",
            &["Type", "Name", "Version", "Source", "Reason"],
            &rows,
        )
    };
    println!("{rendered}");
}
