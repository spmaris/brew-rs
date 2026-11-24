//! Cask removal planning, parking, and rollback.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};

use super::support::{
    is_cross_device, read_cask_receipt, safe_prefix_join_existing_no_symlinks,
    validate_cask_receipt, verify_regular_file_sha256,
};
use super::{
    CaskReceipt, CaskRemovalPlan, CaskRemovalStep, ParkedEntry, caskroom_dir, casks_state_dir,
};
use crate::infra::store::{COUNTER, Store, monotonic_nonce};

pub(super) fn plan_cask_removals(
    store: &Store,
    expected: &[CaskReceipt],
) -> Result<Vec<CaskRemovalPlan>> {
    let prefix = store.prefix();
    let state_dir_path = casks_state_dir(prefix);
    let mut seen = HashSet::new();
    let mut plans = Vec::with_capacity(expected.len());

    for receipt in expected {
        validate_cask_receipt(receipt)?;
        if !seen.insert(receipt.token.as_str()) {
            bail!(
                "cask '{}' appears more than once in removal list",
                receipt.token
            );
        }

        let receipt_path = state_dir_path.join(format!("{}.json", receipt.token));
        let current = read_cask_receipt(&receipt_path).with_context(|| {
            format!(
                "refusing to remove cask '{} {}': receipt '{}' could not be read",
                receipt.token,
                receipt.version,
                receipt_path.display()
            )
        })?;
        if current != *receipt {
            bail!(
                "refusing to remove cask '{} {}': receipt changed on disk",
                receipt.token,
                receipt.version
            );
        }

        let mut steps = vec![CaskRemovalStep::Receipt(receipt_path)];

        for link in &receipt.links {
            let link_path = safe_prefix_join_existing_no_symlinks(prefix, &link.dest)?;
            let meta = fs::symlink_metadata(&link_path).with_context(|| {
                format!(
                    "refusing to remove cask '{}': owned symlink '{}' is missing",
                    receipt.token,
                    link_path.display()
                )
            })?;
            if !meta.file_type().is_symlink() {
                bail!(
                    "refusing to remove cask '{}': '{}' is no longer a symlink",
                    receipt.token,
                    link_path.display()
                );
            }
            let actual_target = fs::read_link(&link_path).with_context(|| {
                format!("failed to read cask symlink '{}'", link_path.display())
            })?;
            if actual_target != Path::new(&link.target) {
                bail!(
                    "refusing to remove cask '{}': symlink '{}' points to '{}' instead of '{}'",
                    receipt.token,
                    link_path.display(),
                    actual_target.display(),
                    link.target
                );
            }
            steps.push(CaskRemovalStep::Link {
                path: link_path,
                expected_target: link.target.clone(),
            });
        }

        for wrapper in &receipt.wrappers {
            let wrapper_path = safe_prefix_join_existing_no_symlinks(prefix, &wrapper.dest)?;
            verify_regular_file_sha256(&wrapper_path, &wrapper.sha256).with_context(|| {
                format!(
                    "refusing to remove cask '{}': command_wrapper '{}' was modified or removed",
                    receipt.token,
                    wrapper_path.display()
                )
            })?;
            steps.push(CaskRemovalStep::Wrapper {
                path: wrapper_path,
                expected_sha256: wrapper.sha256.clone(),
            });
        }

        for app in &receipt.apps {
            let Some(parent) = app.path.parent() else {
                bail!("invalid app path '{}' in receipt", app.path.display());
            };
            let parent_meta = fs::symlink_metadata(parent)
                .with_context(|| format!("failed to inspect appdir '{}'", parent.display()))?;
            if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
                bail!(
                    "appdir '{}' is no longer a real directory",
                    parent.display()
                );
            }
            let app_meta = fs::symlink_metadata(&app.path).with_context(|| {
                format!(
                    "refusing to remove cask '{}': app bundle '{}' is missing",
                    receipt.token,
                    app.path.display()
                )
            })?;
            if app_meta.file_type().is_symlink() || !app_meta.is_dir() {
                bail!(
                    "refusing to remove cask '{}': '{}' is not a real .app directory",
                    receipt.token,
                    app.path.display()
                );
            }
            steps.push(CaskRemovalStep::AppDir(app.path.clone()));
        }

        let pkg_dir = caskroom_dir(prefix).join(&receipt.token);
        let mut prune_pkg = None;
        if !receipt.caskroom_files.is_empty() {
            let ver_dir = pkg_dir.join(&receipt.version);
            let meta = fs::symlink_metadata(&ver_dir).with_context(|| {
                format!(
                    "refusing to remove cask '{}': Caskroom version '{}' is missing",
                    receipt.token,
                    ver_dir.display()
                )
            })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!(
                    "refusing to remove cask '{}': '{}' is not a real directory",
                    receipt.token,
                    ver_dir.display()
                );
            }
            steps.push(CaskRemovalStep::CaskroomVersionDir(ver_dir));
            prune_pkg = Some(pkg_dir);
        }

        plans.push(CaskRemovalPlan { steps, prune_pkg });
    }
    Ok(plans)
}

pub(super) fn apply_removals(
    plans: &[CaskRemovalPlan],
    quarantine: &Path,
    parked: &mut Vec<ParkedEntry>,
) -> Result<()> {
    for plan in plans {
        for step in &plan.steps {
            match step {
                CaskRemovalStep::Receipt(path) => {
                    park_same_fs_or_sibling(path, quarantine, parked, false)?;
                }
                CaskRemovalStep::Link {
                    path,
                    expected_target,
                } => {
                    let meta = fs::symlink_metadata(path)?;
                    if !meta.file_type().is_symlink()
                        || fs::read_link(path)? != Path::new(expected_target)
                    {
                        bail!("cask symlink '{}' changed before removal", path.display());
                    }
                    park_same_fs_or_sibling(path, quarantine, parked, false)?;
                }
                CaskRemovalStep::Wrapper {
                    path,
                    expected_sha256,
                } => {
                    verify_regular_file_sha256(path, expected_sha256)?;
                    park_same_fs_or_sibling(path, quarantine, parked, false)?;
                }
                CaskRemovalStep::CaskroomVersionDir(path) | CaskRemovalStep::AppDir(path) => {
                    let meta = fs::symlink_metadata(path)?;
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        bail!(
                            "cask directory '{}' is no longer a real directory",
                            path.display()
                        );
                    }
                    park_same_fs_or_sibling(path, quarantine, parked, true)?;
                }
            }
        }
        if let Some(ref pkg_dir) = plan.prune_pkg
            && let Ok(()) = fs::remove_dir(pkg_dir)
        {
            parked.push(ParkedEntry::PrunedDir(pkg_dir.clone()));
        }
    }
    Ok(())
}

fn park_same_fs_or_sibling(
    source: &Path,
    quarantine: &Path,
    parked: &mut Vec<ParkedEntry>,
    allow_cross_device_sibling: bool,
) -> Result<()> {
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let qdest = quarantine.join(format!("item-{seq}"));
    match fs::rename(source, &qdest) {
        Ok(()) => {
            parked.push(ParkedEntry::Quarantined {
                source: source.to_path_buf(),
                parked: qdest,
            });
            Ok(())
        }
        Err(err) if allow_cross_device_sibling && is_cross_device(&err) => {
            let parent = source
                .parent()
                .context("app bundle path has no parent directory")?;
            let sibling = parent.join(format!(
                ".brewrs-parked-{}-{}-{seq}",
                std::process::id(),
                monotonic_nonce()
            ));
            fs::rename(source, &sibling).with_context(|| {
                format!(
                    "failed to move '{}' to temporary removal path '{}'",
                    source.display(),
                    sibling.display()
                )
            })?;
            parked.push(ParkedEntry::CrossDeviceSibling {
                source: source.to_path_buf(),
                sibling,
            });
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to quarantine '{}' during cask removal",
                source.display()
            )
        }),
    }
}

pub(super) fn rollback_parked(parked: &[ParkedEntry]) -> Vec<String> {
    let mut issues = Vec::new();
    for entry in parked.iter().rev() {
        match entry {
            ParkedEntry::Quarantined { source, parked } => {
                if let Err(err) = fs::rename(parked, source) {
                    issues.push(format!("failed to restore '{}': {err}", source.display()));
                }
            }
            ParkedEntry::CrossDeviceSibling { source, sibling } => {
                if let Err(err) = fs::rename(sibling, source) {
                    issues.push(format!("failed to restore '{}': {err}", source.display()));
                }
            }
            ParkedEntry::PrunedDir(dir) => {
                if let Err(err) = fs::create_dir(dir) {
                    issues.push(format!("failed to recreate '{}': {err}", dir.display()));
                }
            }
        }
    }
    issues
}

pub(super) fn cleanup_parked_siblings(parked: &[ParkedEntry]) {
    for entry in parked {
        if let ParkedEntry::CrossDeviceSibling { sibling, .. } = entry {
            let _ = fs::remove_dir_all(sibling);
        }
    }
}
