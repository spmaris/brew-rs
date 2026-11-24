//! Native Homebrew cask installation, listing, upgrade, and removal.
//!
//! Operates strictly on the isolated brewrs prefix (defaulting `.app` bundles to
//! `<prefix>/Applications` unless an explicit safe `--appdir` is supplied) and
//! tracks installed casks via `.brewrs/state/casks/<token>.json`.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    config::Config,
    ui::{
        DnfStyleFormatter,
        table::{Row, Section, display_path},
    },
};

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
                "Cannot install cask {} natively: {}.",
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
    let mut installing = Vec::new();
    let mut present = Vec::new();
    for cask in &plan {
        let pending_cask = pending.iter().any(|item| item.token == cask.token);
        let cells = vec![
            cask.token.clone(),
            cask.version.clone(),
            summarize_artifacts(&cask.artifacts),
        ];
        if pending_cask {
            installing.push(Row::plan("install", String::new(), cells));
        } else {
            present.push(Row::plan("already-installed", String::new(), cells));
        }
    }
    let count = installing.len();
    let mut summary = Vec::new();
    if count != 0 {
        summary.push((
            "Install",
            format!("{count} {}", if count == 1 { "cask" } else { "casks" }),
        ));
    }
    if prefix.is_some() {
        summary.push(("Prefix", display_path(store.prefix())));
    }
    summary.push(("Appdir", resolved_appdir.display().to_string()));
    let rendered = formatter.plan_table(
        "Cask install plan",
        &["Cask", "Version", "Artifacts"],
        vec![
            Section::new("Installing", installing),
            Section::new("Already installed", present),
        ],
        &summary,
    );
    println!("{}", rendered.table);
    if let Some(summary) = rendered.summary {
        eprintln!("{summary}");
    }

    if pending.is_empty() && !dry_run {
        formatter.note("\nNothing to install.");
        return Ok(());
    }
    if dry_run {
        let count = pending.len();
        if count == 0 {
            formatter.note("\nDry run: nothing to do.");
        } else {
            let noun = if count == 1 { "change" } else { "changes" };
            formatter.note(&format!(
                "\nDry run: {count} {noun} planned, nothing applied."
            ));
        }
        return Ok(());
    }
    io::stdout().flush()?;
    let count = pending.len();
    let question = format!(
        "Install {count} {}?",
        if count == 1 { "cask" } else { "casks" }
    );
    if config.general.confirm_operations && !yes && !confirm(&question)? {
        eprintln!("Cancelled. No packages changed.");
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

    formatter.note(&format!(
        "\n{}",
        formatter.result(&format!(
            "Installed {count} {}",
            if count == 1 { "cask" } else { "casks" }
        ))
    ));
    let mut follow_up = Vec::new();
    if any_apps {
        follow_up.push(("Appdir", display_path(&resolved_appdir)));
    }
    if any_bin {
        follow_up.push(("Bin", display_path(&store.prefix().join("bin"))));
    }
    if !follow_up.is_empty() {
        formatter.note(&formatter.fields(&follow_up));
    }
    Ok(())
}

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
    println!(
        "{}",
        formatter.table(
            &format!("Casks ({})", rows.len()),
            &["Cask", "Version", "Targets"],
            &rows,
        )
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
        let count = pending.len();
        if count == 0 {
            formatter.note("\nDry run: nothing to do.");
        } else {
            let noun = if count == 1 { "change" } else { "changes" };
            formatter.note(&format!(
                "\nDry run: {count} {noun} planned, nothing applied."
            ));
        }
        return Ok(());
    }
    io::stdout().flush()?;
    let question = format!(
        "Remove {} {}?",
        pending.len(),
        if pending.len() == 1 { "cask" } else { "casks" }
    );
    if config.general.confirm_operations && !yes && !confirm(&question)? {
        eprintln!("Cancelled. No packages changed.");
        return Ok(());
    }
    let count = pending.len();
    if count == 0 {
        cask_store::remove(&store, &pending)?;
    } else {
        eprintln!();
        let noun = if count == 1 { "cask" } else { "casks" };
        let steps = crate::ui::steps::Steps::new(config);
        let step = steps.start(&format!("Unlinking {count} {noun}"));
        match cask_store::remove(&store, &pending) {
            Ok(()) => step.done(&format!("Unlinked {count} {noun}")),
            Err(error) => {
                step.fail();
                return Err(error);
            }
        }
    }
    formatter.note(&format!(
        "\n{}",
        formatter.result(&format!(
            "Removed {} {}",
            pending.len(),
            if pending.len() == 1 { "cask" } else { "casks" }
        ))
    ));
    Ok(())
}

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

fn print_auto_update_skip_note(formatter: &DnfStyleFormatter, skipped: usize) {
    if skipped > 0 {
        formatter.note(&format!(
            "Skipped {skipped} auto-updating {}; pass --greedy to include them.",
            if skipped == 1 { "cask" } else { "casks" }
        ));
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
