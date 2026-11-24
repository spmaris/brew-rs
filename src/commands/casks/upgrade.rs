use std::collections::HashSet;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::commands::install::{confirm, prefix_path};
use crate::config::Config;
use crate::domain::cask_plan;
use crate::domain::cask_plan::{CaskVersionOrder, PlannedCask, compare_cask_versions};
use crate::infra::cask_store::CaskReceipt;
use crate::infra::platform::default_target;
use crate::infra::store::{Store, reject_protected_prefix};
use crate::infra::{cask_download, cask_store};
use crate::ui::DnfStyleFormatter;
use crate::ui::table::{Row, Section, display_path};

use super::add::resolve;
use super::stage::{preview_receipt_for, stage_cask};
use crate::ui::text::short_digest;

pub(crate) async fn upgrade(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    greedy: bool,
    dry_run: bool,
    yes: bool,
    announce_current: bool,
) -> Result<bool> {
    if std::env::consts::OS != "macos" {
        bail!("Native cask upgrades are supported on macOS only");
    }
    let path = prefix_path(prefix)?;
    reject_protected_prefix(&path)?;
    let store = Store::open(&path, false)?;
    let installed = cask_store::installed(&store)?;
    let selected = select_installed_casks(packages, &installed)?;
    let formatter = DnfStyleFormatter::new(config);
    if selected.is_empty() {
        formatter.note("No casks installed yet.");
        return Ok(false);
    }

    let (_, changes, skipped_auto) =
        resolve_cask_changes(config, &store, &installed, &selected, greedy).await?;
    if changes.is_empty() {
        if announce_current {
            formatter.note("All casks are up to date.");
        }
        print_auto_update_skip_note(&formatter, skipped_auto);
        return Ok(false);
    }

    render_cask_upgrade_preview(
        &formatter,
        "Cask upgrade plan",
        store.prefix(),
        prefix.is_some(),
        &changes,
    );
    print_auto_update_skip_note(&formatter, skipped_auto);
    if dry_run {
        let count = changes.len();
        let noun = if count == 1 { "change" } else { "changes" };
        formatter.note(&format!(
            "\nDry run: {count} {noun} planned, nothing applied."
        ));
        return Ok(true);
    }
    io::stdout().flush()?;
    let question = format!(
        "Upgrade {} {}?",
        changes.len(),
        if changes.len() == 1 { "cask" } else { "casks" }
    );
    if config.general.confirm_operations && !yes && !confirm(&question)? {
        eprintln!("Cancelled. No packages changed.");
        return Ok(true);
    }

    let mut downloaded = Vec::with_capacity(changes.len());
    for change in changes {
        let download_path =
            cask_download::fetch(config, &change.planned.url, &change.planned.sha256)
                .await
                .with_context(|| {
                    format!(
                        "Failed to fetch cask {} {}",
                        change.planned.token, change.planned.version
                    )
                })?;
        downloaded.push((change, download_path));
    }

    let store = Store::open_existing(&path)?;
    let mut replacements = Vec::with_capacity(downloaded.len());
    for (change, download_path) in downloaded {
        let prepared = stage_cask(
            &store,
            &change.planned,
            &change.appdir,
            change.old.appdir.is_some(),
            &download_path,
        )?;
        replacements.push((change.old, prepared));
    }

    let count = replacements.len();
    cask_store::upgrade(&store, replacements)?;
    formatter.note(&format!(
        "\n{}",
        formatter.result(&format!(
            "Upgraded {count} {}",
            if count == 1 { "cask" } else { "casks" }
        ))
    ));
    Ok(true)
}

pub(crate) async fn check_updates(
    config: &Config,
    prefix: Option<&Path>,
    greedy: bool,
    quiet_if_empty: bool,
) -> Result<bool> {
    if std::env::consts::OS != "macos" {
        bail!("Native cask upgrades are supported on macOS only");
    }
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let installed = cask_store::installed(&store)?;
    let formatter = DnfStyleFormatter::new(config);
    if installed.is_empty() {
        if !quiet_if_empty {
            formatter.note("No casks installed yet.");
        }
        return Ok(false);
    }

    let (_, changes, skipped_auto) =
        resolve_cask_changes(config, &store, &installed, &installed, greedy).await?;
    if changes.is_empty() {
        if !quiet_if_empty {
            formatter.note("All casks are up to date.");
        }
        print_auto_update_skip_note(&formatter, skipped_auto);
        return Ok(false);
    }
    render_cask_upgrade_preview(
        &formatter,
        "Available cask updates",
        store.prefix(),
        prefix.is_some(),
        &changes,
    );
    print_auto_update_skip_note(&formatter, skipped_auto);
    formatter.note(&format!(
        "\n{} cask {} available.",
        formatter.success(&changes.len().to_string()),
        if changes.len() == 1 {
            "update"
        } else {
            "updates"
        }
    ));
    Ok(true)
}

pub(super) struct CaskUpgradeChange {
    pub(super) old: CaskReceipt,
    pub(super) planned: PlannedCask,
    pub(super) appdir: PathBuf,
}

pub(super) fn select_installed_casks(
    packages: &[String],
    installed: &[CaskReceipt],
) -> Result<Vec<CaskReceipt>> {
    if packages.is_empty() {
        return Ok(installed.to_vec());
    }
    let mut selected = HashSet::new();
    let mut result = Vec::with_capacity(packages.len());
    for raw in packages {
        let token = cask_plan::parse_cask_target(raw)?;
        if selected.insert(token) {
            let receipt = installed
                .iter()
                .find(|entry| entry.token == token)
                .with_context(|| {
                    format!("'{token}' is not installed as a cask in this brewrs prefix")
                })?;
            result.push(receipt.clone());
        }
    }
    Ok(result)
}

pub(super) async fn resolve_cask_changes(
    config: &Config,
    store: &Store,
    installed: &[CaskReceipt],
    selected: &[CaskReceipt],
    greedy: bool,
) -> Result<(String, Vec<CaskUpgradeChange>, usize)> {
    let mut skipped_auto = 0_usize;
    let mut to_check = Vec::new();
    for receipt in selected {
        if receipt.auto_updates && !greedy {
            skipped_auto += 1;
        } else {
            to_check.push(receipt.clone());
        }
    }

    let tag = default_target()?;
    if to_check.is_empty() {
        return Ok((tag, Vec::new(), skipped_auto));
    }

    let tokens: Vec<String> = to_check.iter().map(|r| r.token.clone()).collect();
    let plans = resolve(config, &tokens, &tag).await?;
    let existing_formulae = store.installed()?;
    let mut changes = Vec::new();
    let mut projected = installed.to_vec();

    for old in &to_check {
        let new = plans
            .iter()
            .find(|p| p.token == old.token)
            .with_context(|| {
                format!(
                    "No cask metadata resolved for installed cask '{}'",
                    old.token
                )
            })?;

        match compare_cask_versions(&new.version, &old.version) {
            CaskVersionOrder::Same => {
                if new.sha256 == old.sha256 {
                    continue;
                }
                bail!(
                    "cask {} {} is installed with sha256 {}, but the repository resolves {} at the same version; refusing to replace it in place",
                    old.token,
                    old.version,
                    short_digest(&old.sha256),
                    short_digest(&new.sha256)
                );
            }
            CaskVersionOrder::Downgrade => {
                bail!(
                    "Refusing to downgrade cask {} from installed {} to available {}",
                    old.token,
                    old.version,
                    new.version
                );
            }
            CaskVersionOrder::Upgrade => {
                if !new.install_blockers.is_empty() {
                    bail!(
                        "Cannot upgrade cask {} natively: {}.",
                        old.token,
                        new.install_blockers.join("; ")
                    );
                }
                let appdir = cask_store::resolve_appdir(store.prefix(), old.appdir.as_deref())?;
                let preview = preview_receipt_for(new, &appdir, old.appdir.is_some());
                if let Some(slot) = projected.iter_mut().find(|r| r.token == old.token) {
                    *slot = preview;
                }
                changes.push(CaskUpgradeChange {
                    old: old.clone(),
                    planned: new.clone(),
                    appdir,
                });
            }
        }
    }

    cask_store::validate_cask_conflicts(&projected, &existing_formulae)?;
    Ok((tag, changes, skipped_auto))
}

pub(super) fn render_cask_upgrade_preview(
    formatter: &DnfStyleFormatter,
    title: &str,
    prefix: &Path,
    show_prefix: bool,
    changes: &[CaskUpgradeChange],
) {
    let rows: Vec<_> = changes
        .iter()
        .map(|change| {
            Row::plan(
                "upgrade",
                String::new(),
                vec![
                    change.old.token.clone(),
                    format!("{} → {}", change.old.version, change.planned.version),
                ],
            )
        })
        .collect();
    let mut summary = vec![(
        "Upgrade",
        format!(
            "{} {}",
            changes.len(),
            if changes.len() == 1 { "cask" } else { "casks" }
        ),
    )];
    if show_prefix {
        summary.push(("Prefix", display_path(prefix)));
    }
    let rendered = formatter.plan_table(
        title,
        &["Cask", "Version"],
        vec![Section::new("Upgrading", rows)],
        &summary,
    );
    println!("{}", rendered.table);
    if let Some(summary) = rendered.summary {
        eprintln!("{summary}");
    }
}

pub(super) fn print_auto_update_skip_note(formatter: &DnfStyleFormatter, skipped: usize) {
    if skipped > 0 {
        formatter.note(&format!(
            "Skipped {skipped} auto-updating {}; pass --greedy to include them.",
            if skipped == 1 { "cask" } else { "casks" }
        ));
    }
}
