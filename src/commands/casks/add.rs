use std::io;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::commands::install::{confirm, installed_summary, prefix_path};
use crate::config::Config;
use crate::domain::cask_plan;
use crate::domain::cask_plan::PlannedCask;
use crate::infra::cask_source::HttpCaskSource;
use crate::infra::platform::default_target;
use crate::infra::store::{Store, reject_protected_prefix};
use crate::infra::{cask_download, cask_store};
use crate::ui::DnfStyleFormatter;
use crate::ui::table::{Row, Section, display_path};

use super::stage::{
    cask_replacement_conflict, preview_receipt_for, stage_cask, summarize_artifacts,
};

/// Resolve cask install plans for `targets` on host tag `tag` from the
/// official cask API.
pub(super) async fn resolve(
    config: &Config,
    targets: &[String],
    tag: &str,
) -> Result<Vec<PlannedCask>> {
    let source = HttpCaskSource::new(config)?;
    cask_plan::resolve_with(&source, targets, tag).await
}

/// Read-only cask planner reused across `migrate --check`.
pub(crate) struct AuditCaskPlanner {
    pub(super) source: HttpCaskSource,
}

impl AuditCaskPlanner {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            source: HttpCaskSource::read_only(config)?,
        })
    }

    pub(crate) async fn resolve_one(&self, token: &str, tag: &str) -> Result<PlannedCask> {
        let mut plan = cask_plan::resolve_with(&self.source, &[token.to_string()], tag).await?;
        plan.pop()
            .context("cask planner returned an empty plan for one target")
    }
}

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
    let plan = resolve(config, packages, &tag).await?;

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

    let installed: Vec<String> = prepared
        .iter()
        .map(|p| format!("{} {}", p.receipt.token, p.receipt.version))
        .collect();
    let any_apps = prepared.iter().any(|p| !p.receipt.apps.is_empty());
    let any_bin = prepared.iter().any(|p| {
        p.receipt.links.iter().any(|l| l.dest.starts_with("bin/")) || !p.receipt.wrappers.is_empty()
    });
    cask_store::commit(&store, prepared)?;

    formatter.note(&format!(
        "\n{}",
        formatter.result(&installed_summary(&installed, 0))
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
