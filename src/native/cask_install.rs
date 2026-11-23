//! Native Homebrew cask installation, listing, upgrade, and removal.
//!
//! Operates strictly on the isolated brewrs prefix (defaulting `.app` bundles to
//! `<prefix>/Applications` unless an explicit safe `--appdir` is supplied) and
//! tracks installed casks via `.brewrs/state/casks/<token>.json`.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{config::Config, ui::DnfStyleFormatter};

use super::cask_extract::{self, StagedCaskPayload};
use super::cask_plan::{CaskVersionOrder, PlannedCask, PlannedCaskArtifact, compare_cask_versions};
use super::cask_store::{
    self, CaskReceipt, InstalledApp, InstalledCaskLink, InstalledWrapper, PreparedCask,
};
use super::install::{confirm, prefix_path};
use super::store::{Store, reject_protected_prefix};
use super::{cask_download, cask_plan, default_target};

pub(crate) async fn install(
    config: &Config,
    prefix: Option<&Path>,
    appdir: Option<&Path>,
    packages: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if std::env::consts::OS != "macos" {
        bail!("Native cask installation is supported on macOS only");
    }
    let prefix_dir = prefix_path(prefix)?;
    reject_protected_prefix(&prefix_dir)?;
    let store = Store::open(&prefix_dir, false)?;
    let resolved_appdir = cask_store::resolve_appdir(&prefix_dir, appdir)?;
    let tag = default_target()?;
    let existing_casks = cask_store::installed(&store)?;
    let existing_formulae = store.installed()?;
    let plan = cask_plan::resolve(config, packages, &tag).await?;

    let mut pending = Vec::new();
    let mut projected = existing_casks.clone();

    for cask in &plan {
        if let Some(installed) = existing_casks
            .iter()
            .find(|entry| entry.token == cask.token)
        {
            if installed.version == cask.version && installed.sha256 == cask.sha256 {
                continue;
            }
            bail!("{}", cask_replacement_conflict(installed, cask));
        }
        if !cask.install_blockers.is_empty() {
            bail!(
                "Cannot install cask {} natively: {}",
                cask.token,
                cask.install_blockers.join("; ")
            );
        }
        let preview_receipt = preview_receipt_for(cask, &resolved_appdir, appdir.is_some());
        projected.push(preview_receipt);
        pending.push(cask);
    }

    cask_store::validate_cask_conflicts(&projected, &existing_formulae)?;

    let formatter = DnfStyleFormatter::new(config);
    let rows: Vec<_> = plan
        .iter()
        .map(|cask| {
            let action = if pending.iter().any(|item| item.token == cask.token) {
                "Install"
            } else {
                "Already installed"
            };
            vec![
                cask.token.clone(),
                cask.version.clone(),
                action.into(),
                summarize_artifacts(&cask.artifacts),
            ]
        })
        .collect();

    println!(
        "{}  {}",
        formatter.section("Native cask install"),
        formatter.info(&tag)
    );
    println!(
        "{}",
        formatter.table(&["Cask", "Version", "Action", "Artifacts"], &rows)
    );
    println!(
        "{}",
        formatter.field("Prefix", &store.prefix().display().to_string())
    );
    println!(
        "{}",
        formatter.field("Appdir", &resolved_appdir.display().to_string())
    );

    if pending.is_empty() {
        println!("\nNothing to install.");
        return Ok(());
    }
    if dry_run {
        let noun = if pending.len() == 1 { "cask" } else { "casks" };
        println!("\n{} {noun} planned. No packages changed.", pending.len());
        return Ok(());
    }
    io::stdout().flush()?;
    if config.general.confirm_operations && !yes && !confirm()? {
        println!("Cancelled. No packages changed.");
        return Ok(());
    }

    let mut downloaded = Vec::with_capacity(pending.len());
    for cask in &pending {
        let path = cask_download::fetch(config, &cask.url, &cask.sha256)
            .await
            .with_context(|| format!("Failed to fetch cask {} {}", cask.token, cask.version))?;
        downloaded.push((cask, path));
    }

    let store = Store::open(&prefix_dir, true)?;
    let mut prepared = Vec::with_capacity(downloaded.len());
    for (cask, download_path) in downloaded {
        prepared.push(stage_cask(
            &store,
            cask,
            &resolved_appdir,
            appdir.is_some(),
            &download_path,
        )?);
    }

    let count = prepared.len();
    let any_apps = prepared.iter().any(|p| !p.receipt.apps.is_empty());
    let any_bin = prepared.iter().any(|p| {
        p.receipt.links.iter().any(|l| l.dest.starts_with("bin/")) || !p.receipt.wrappers.is_empty()
    });
    cask_store::commit(&store, prepared)?;

    println!(
        "\n{} {count} {}.",
        formatter.success("Installed"),
        if count == 1 { "cask" } else { "casks" }
    );
    if any_apps {
        println!(
            "{}",
            formatter.field("Appdir", &resolved_appdir.display().to_string())
        );
    }
    if any_bin {
        println!(
            "{}",
            formatter.field("Bin", &store.prefix().join("bin").display().to_string())
        );
    }
    Ok(())
}

pub(crate) fn list(config: &Config, prefix: Option<&Path>) -> Result<()> {
    let path = prefix_path(prefix)?;
    let store = Store::open(&path, false)?;
    let casks = cask_store::installed(&store)?;
    if casks.is_empty() {
        println!("No brewrs-managed casks in {}.", store.prefix().display());
        return Ok(());
    }
    let formatter = DnfStyleFormatter::new(config);
    render_cask_list(&formatter, &casks);
    Ok(())
}

pub(super) fn render_cask_list(formatter: &DnfStyleFormatter, casks: &[CaskReceipt]) {
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
    println!("{}", formatter.section("Native casks"));
    println!(
        "{}",
        formatter.table(&["Cask", "Version", "Targets"], &rows)
    );
}

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
            vec![
                receipt.token.clone(),
                receipt.version.clone(),
                "Remove".into(),
            ]
        })
        .collect();
    println!("{}", formatter.section("Remove casks"));
    println!("{}", formatter.table(&["Cask", "Version", "Action"], &rows));
    println!(
        "{}",
        formatter.field("Prefix", &store.prefix().display().to_string())
    );
    if dry_run {
        println!("\nNo packages changed.");
        return Ok(());
    }
    io::stdout().flush()?;
    if config.general.confirm_operations && !yes && !confirm()? {
        println!("Cancelled. No packages changed.");
        return Ok(());
    }
    cask_store::remove(&store, &pending)?;
    println!(
        "\n{} {} {}.",
        formatter.success("Removed"),
        pending.len(),
        if pending.len() == 1 { "cask" } else { "casks" }
    );
    Ok(())
}

pub(crate) async fn upgrade(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    greedy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if std::env::consts::OS != "macos" {
        bail!("Native cask upgrades are supported on macOS only");
    }
    let path = prefix_path(prefix)?;
    reject_protected_prefix(&path)?;
    let store = Store::open(&path, false)?;
    let installed = cask_store::installed(&store)?;
    let selected = select_installed_casks(packages, &installed)?;
    if selected.is_empty() {
        println!("No brewrs-managed casks to upgrade.");
        return Ok(());
    }

    let (tag, changes, skipped_auto) =
        resolve_cask_changes(config, &store, &installed, &selected, greedy).await?;
    if changes.is_empty() {
        println!("All selected casks are up to date.");
        print_auto_update_skip_note(skipped_auto);
        return Ok(());
    }

    let formatter = DnfStyleFormatter::new(config);
    render_cask_upgrade_preview(
        &formatter,
        "Native cask upgrade",
        &tag,
        store.prefix(),
        &changes,
    );
    print_auto_update_skip_note(skipped_auto);
    if dry_run {
        println!("\nNo packages changed.");
        return Ok(());
    }
    io::stdout().flush()?;
    if config.general.confirm_operations && !yes && !confirm()? {
        println!("Cancelled. No packages changed.");
        return Ok(());
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
    println!(
        "\n{} {count} {}.",
        formatter.success("Upgraded"),
        if count == 1 { "cask" } else { "casks" }
    );
    Ok(())
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
    if installed.is_empty() {
        if !quiet_if_empty {
            println!("No brewrs-managed casks to upgrade.");
        }
        return Ok(false);
    }

    let (tag, changes, skipped_auto) =
        resolve_cask_changes(config, &store, &installed, &installed, greedy).await?;
    let formatter = DnfStyleFormatter::new(config);
    if changes.is_empty() {
        if !quiet_if_empty {
            println!("All brewrs-managed casks are up to date.");
        }
        print_auto_update_skip_note(skipped_auto);
        return Ok(false);
    }
    render_cask_upgrade_preview(
        &formatter,
        "Native cask updates",
        &tag,
        store.prefix(),
        &changes,
    );
    print_auto_update_skip_note(skipped_auto);
    println!(
        "\n{} cask {} available.",
        formatter.success(&changes.len().to_string()),
        if changes.len() == 1 {
            "update"
        } else {
            "updates"
        }
    );
    Ok(true)
}

struct CaskUpgradeChange {
    old: CaskReceipt,
    planned: PlannedCask,
    appdir: PathBuf,
}

fn select_installed_casks(
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

async fn resolve_cask_changes(
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
    let plans = cask_plan::resolve(config, &tokens, &tag).await?;
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
                        "Cannot upgrade cask {} natively: {}",
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

fn stage_cask(
    store: &Store,
    planned: &PlannedCask,
    appdir: &Path,
    explicit_appdir: bool,
    download_path: &Path,
) -> Result<PreparedCask> {
    let stage = store.stage()?;
    let staged: StagedCaskPayload =
        cask_extract::stage(download_path, stage.path(), planned, store.prefix(), appdir)
            .with_context(|| {
                format!("Failed to stage cask {} {}", planned.token, planned.version)
            })?;

    let receipt = CaskReceipt {
        token: planned.token.clone(),
        version: planned.version.clone(),
        sha256: planned.sha256.clone(),
        auto_updates: planned.auto_updates,
        appdir: if explicit_appdir {
            Some(appdir.to_path_buf())
        } else {
            None
        },
        apps: staged
            .app_bundles
            .iter()
            .map(|app| InstalledApp {
                bundle_name: app.bundle_name.clone(),
                path: appdir.join(&app.bundle_name),
            })
            .collect(),
        caskroom_files: staged.caskroom_files.clone(),
        links: staged
            .symlinks
            .iter()
            .map(|link| InstalledCaskLink {
                dest: link.relative_dest.clone(),
                target: link.target.clone(),
            })
            .collect(),
        wrappers: staged
            .wrappers
            .iter()
            .map(|wrapper| InstalledWrapper {
                dest: wrapper.relative_dest.clone(),
                sha256: wrapper.sha256.clone(),
            })
            .collect(),
        conflicts_with_casks: planned.conflicts_with_casks.clone(),
        conflicts_with_formulae: planned.conflicts_with_formulae.clone(),
    };

    Ok(PreparedCask {
        receipt,
        staged,
        stage,
    })
}

fn preview_receipt_for(planned: &PlannedCask, appdir: &Path, explicit_appdir: bool) -> CaskReceipt {
    let apps = planned
        .artifacts
        .iter()
        .filter_map(|artifact| match artifact {
            PlannedCaskArtifact::App { bundle_name, .. } => Some(InstalledApp {
                bundle_name: bundle_name.clone(),
                path: appdir.join(bundle_name),
            }),
            _ => None,
        })
        .collect();
    CaskReceipt {
        token: planned.token.clone(),
        version: planned.version.clone(),
        sha256: planned.sha256.clone(),
        auto_updates: planned.auto_updates,
        appdir: if explicit_appdir {
            Some(appdir.to_path_buf())
        } else {
            None
        },
        apps,
        caskroom_files: Vec::new(),
        links: Vec::new(),
        wrappers: Vec::new(),
        conflicts_with_casks: planned.conflicts_with_casks.clone(),
        conflicts_with_formulae: planned.conflicts_with_formulae.clone(),
    }
}

fn summarize_artifacts(artifacts: &[PlannedCaskArtifact]) -> String {
    let parts: Vec<String> = artifacts
        .iter()
        .map(|artifact| match artifact {
            PlannedCaskArtifact::App { bundle_name, .. } => bundle_name.clone(),
            PlannedCaskArtifact::Binary { link_name, .. }
            | PlannedCaskArtifact::CommandWrapper { link_name, .. } => format!("bin/{link_name}"),
            PlannedCaskArtifact::PrefixLink { relative_dest, .. } => relative_dest.clone(),
        })
        .collect();
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(", ")
    }
}

fn summarize_receipt_targets(receipt: &CaskReceipt) -> String {
    let mut parts: Vec<String> = receipt.apps.iter().map(|a| a.bundle_name.clone()).collect();
    for link in &receipt.links {
        parts.push(link.dest.clone());
    }
    for wrapper in &receipt.wrappers {
        parts.push(wrapper.dest.clone());
    }
    if parts.is_empty() {
        "Caskroom".into()
    } else {
        parts.join(", ")
    }
}

fn render_cask_upgrade_preview(
    formatter: &DnfStyleFormatter,
    title: &str,
    tag: &str,
    prefix: &Path,
    changes: &[CaskUpgradeChange],
) {
    println!("{}  {}", formatter.section(title), formatter.info(tag));
    let rows: Vec<_> = changes
        .iter()
        .map(|change| {
            vec![
                change.old.token.clone(),
                change.old.version.clone(),
                change.planned.version.clone(),
            ]
        })
        .collect();
    println!(
        "{}",
        formatter.table(&["Cask", "Installed", "Available"], &rows)
    );
    println!(
        "{}",
        formatter.field("Prefix", &prefix.display().to_string())
    );
}

fn print_auto_update_skip_note(skipped: usize) {
    if skipped > 0 {
        println!(
            "Skipped {skipped} auto-updating {}; pass --greedy to include them.",
            if skipped == 1 { "cask" } else { "casks" }
        );
    }
}

fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

fn cask_replacement_conflict(installed: &CaskReceipt, planned: &PlannedCask) -> String {
    match compare_cask_versions(&planned.version, &installed.version) {
        CaskVersionOrder::Upgrade => format!(
            "cask {} {} is installed but {} is available; run `brewrs upgrade --cask {}` to replace it",
            installed.token, installed.version, planned.version, installed.token
        ),
        CaskVersionOrder::Downgrade => format!(
            "cask {} {} is installed but available version is older ({}); refusing to replace it in place",
            installed.token, installed.version, planned.version
        ),
        CaskVersionOrder::Same => format!(
            "cask {} {} is installed with sha256 {}, but available download has sha256 {}; refusing to replace it in place",
            installed.token,
            installed.version,
            short_digest(&installed.sha256),
            short_digest(&planned.sha256)
        ),
    }
}
