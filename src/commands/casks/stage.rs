use std::path::Path;

use anyhow::{Context, Result};

use crate::domain::cask_plan::{
    CaskVersionOrder, PlannedCask, PlannedCaskArtifact, compare_cask_versions,
};
use crate::infra::cask_extract;
use crate::infra::cask_extract::StagedCaskPayload;
use crate::infra::cask_store::{
    CaskReceipt, InstalledApp, InstalledCaskLink, InstalledWrapper, PreparedCask,
};
use crate::infra::store::Store;
use crate::ui::text::short_digest;

pub(super) fn stage_cask(
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

pub(super) fn preview_receipt_for(
    planned: &PlannedCask,
    appdir: &Path,
    explicit_appdir: bool,
) -> CaskReceipt {
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

pub(super) fn summarize_artifacts(artifacts: &[PlannedCaskArtifact]) -> String {
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

pub(super) fn summarize_receipt_targets(receipt: &CaskReceipt) -> String {
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

pub(super) fn cask_replacement_conflict(installed: &CaskReceipt, planned: &PlannedCask) -> String {
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
