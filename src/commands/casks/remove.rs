use std::collections::HashSet;
use std::io;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use super::labels::remove_labels;
use crate::commands::install::{confirm, prefix_path, removed_summary};
use crate::config::Config;
use crate::domain::cask_plan;
use crate::infra::cask_store;
use crate::infra::store::Store;
use crate::ui::table::{Row, Section, display_path};
use crate::ui::{DnfStyleFormatter, phrases};

pub(crate) fn remove(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let installed = cask_store::installed(&store)?;
    let mut selected = HashSet::new();
    let mut pending = Vec::new();
    for raw in packages {
        let token = cask_plan::parse_cask_target(raw)?;
        if selected.insert(token) {
            let receipt = installed
                .iter()
                .find(|entry| entry.token == token)
                .with_context(|| {
                    format!("'{token}' is not installed as a cask in this brewrs prefix")
                })?;
            pending.push(receipt.clone());
        }
    }

    let formatter = DnfStyleFormatter::new(config);
    let rows: Vec<_> = pending
        .iter()
        .map(|receipt| {
            Row::plan(
                "remove",
                String::new(),
                vec![receipt.token.clone(), receipt.version.clone()],
            )
        })
        .collect();
    let mut summary = vec![(
        "Remove",
        format!(
            "{} {}",
            pending.len(),
            if pending.len() == 1 { "cask" } else { "casks" }
        ),
    )];
    if prefix.is_some() {
        summary.push(("Prefix", display_path(store.prefix())));
    }
    let rendered = formatter.plan_table(
        "Cask remove plan",
        &["Cask", "Version"],
        vec![Section::new("Removing", rows)],
        &summary,
    );
    println!("{}", rendered.table);
    if let Some(summary) = rendered.summary {
        eprintln!("{summary}");
    }
    if dry_run {
        formatter.note(&format!("\n{}", phrases::dry_run_summary(0, pending.len())));
        return Ok(());
    }
    io::stdout().flush()?;
    let question = format!(
        "Remove {} {}?",
        pending.len(),
        if pending.len() == 1 { "cask" } else { "casks" }
    );
    if config.general.confirm_operations && !yes && !confirm(&question)? {
        eprintln!("{}", phrases::cancelled("casks"));
        return Ok(());
    }
    let count = pending.len();
    if count == 0 {
        cask_store::remove(&store, &pending)?;
    } else {
        eprintln!();
        let tokens: Vec<String> = pending.iter().map(|r| r.token.clone()).collect();
        let bundles: Vec<String> = pending
            .iter()
            .flat_map(|r| r.apps.iter().map(|app| app.bundle_name.clone()))
            .collect();
        let (active, finished) = remove_labels(&tokens, &bundles);
        let steps = crate::ui::steps::Steps::new(config);
        let step = steps.start(&active);
        match cask_store::remove(&store, &pending) {
            Ok(()) => step.done(&finished),
            Err(error) => {
                step.fail();
                return Err(error.into());
            }
        }
    }
    let removed: Vec<String> = pending.iter().map(|r| r.token.clone()).collect();
    formatter.note(&format!(
        "\n{}",
        formatter.result(&removed_summary(&removed, &[]))
    ));
    Ok(())
}
