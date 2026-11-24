//! Upgrade application and rollback.

mod plan;

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::commit::rollback;
use super::lock::InstallLock;
use super::paths::{retained_dir, stages_dir, state_dir, sync_directory};
use super::prefix::ensure_directory;
use super::receipt::{Prepared, Receipt};
use super::remove::{move_aside, rollback_removal};
use super::{Applied, AppliedChange, MovedPath, RemovalStep, StepKind, Store, Upgrade};

impl Store {
    /// Replace the exact installed packages in `packages` as one transaction.
    ///
    /// Each tuple carries the receipt a preview observed and the freshly staged
    /// replacement. Every old receipt, keg, and owned `bin` link is rechecked
    /// under the writer lock before anything moves. The replacement must have
    /// the same name and requested flag, a different version, and no collision
    /// with foreign package or `bin` entries. Old paths are quarantined, new
    /// packages are installed, and a failure rolls the new paths back and
    /// restores the old ones while the caller still holds this process.
    ///
    /// The projected receipt graph is validated first. A replacement that
    /// would break a surviving receipt's pin to the exact old version and
    /// digest retains the old keg and receipt instead of releasing them: the
    /// old keg stays in place, the old receipt moves to the retained records,
    /// and the managed links move to the replacement. A surviving pin that no
    /// retained record can satisfy still rejects the upgrade unless the
    /// dependent is upgraded in the same call.
    ///
    /// Like commits and removals, upgrades are not crash-atomic; see the module
    /// documentation for the files an interrupted upgrade can leave behind.
    pub(crate) fn upgrade(&self, packages: Vec<(Receipt, Prepared)>) -> Result<()> {
        self.upgrade_inner(packages, None)
    }

    /// [`Store::upgrade`] with a failure point used by the tests.
    pub(super) fn upgrade_inner(
        &self,
        packages: Vec<(Receipt, Prepared)>,
        fail_after: Option<usize>,
    ) -> Result<()> {
        self.ensure_owned()?;
        if packages.is_empty() {
            return Ok(());
        }
        // The lock covers validation, quarantine, install, and rollback, so no
        // concurrent brewrs writer can change receipts, links, or kegs.
        let _lock = InstallLock::acquire(&self.prefix)?;
        // Ownership may have changed while the lock was being acquired.
        self.ensure_owned()?;
        let upgrades = self.plan_upgrade(packages)?;
        self.apply_upgrade(upgrades, fail_after)
    }

    /// Quarantine the preflighted old paths, apply the replacements, and undo
    /// both sides if the install fails.
    fn apply_upgrade(&self, upgrades: Vec<Upgrade>, fail_after: Option<usize>) -> Result<()> {
        let stages = stages_dir(&self.prefix);
        let quarantine = tempfile::Builder::new()
            .prefix("upgrade-")
            .tempdir_in(&stages)
            .with_context(|| {
                format!(
                    "failed to create an upgrade quarantine below '{}'",
                    stages.display()
                )
            })?;

        let mut moved = Vec::new();
        let move_outcome = (|| -> Result<()> {
            let mut index = 0usize;
            for upgrade in &upgrades {
                if upgrade.retain {
                    // Retain the old keg in place. Only the owned links move
                    // aside (the replacement claims them), and the old receipt
                    // moves into the retained records so the exact old
                    // identity survives. The keg directory is never touched.
                    for link in &upgrade.old.links {
                        move_aside(
                            &RemovalStep {
                                source: link.path.clone(),
                                kind: StepKind::Link {
                                    target: link.target.clone(),
                                },
                            },
                            quarantine.path(),
                            index,
                            &mut moved,
                        )?;
                        index += 1;
                    }
                    retain_receipt(
                        &self.prefix,
                        &upgrade.old_receipt.name,
                        &upgrade.old_receipt.version,
                        &upgrade.old.receipt_path,
                        &mut moved,
                    )?;
                } else {
                    let steps = std::iter::once(RemovalStep {
                        source: upgrade.old.receipt_path.clone(),
                        kind: StepKind::Receipt,
                    })
                    .chain(upgrade.old.links.iter().map(|link| RemovalStep {
                        source: link.path.clone(),
                        kind: StepKind::Link {
                            target: link.target.clone(),
                        },
                    }))
                    .chain(std::iter::once(RemovalStep {
                        source: upgrade.old.keg.clone(),
                        kind: StepKind::Directory,
                    }));
                    for step in steps {
                        move_aside(&step, quarantine.path(), index, &mut moved)?;
                        index += 1;
                    }
                }
            }
            Ok(())
        })();

        if let Err(error) = move_outcome {
            return finish_upgrade(quarantine, &[], &moved, error);
        }

        let pending: Vec<_> = upgrades
            .into_iter()
            .map(|upgrade| upgrade.pending)
            .collect();
        let mut applied = Vec::new();
        match self.apply_inner(pending, fail_after, &mut applied, true) {
            Ok(()) => {
                let mut flushed = HashSet::new();
                for change in &moved {
                    let mut parents: Vec<&Path> = Vec::new();
                    match change {
                        AppliedChange::Moved(moved) => {
                            if let Some(parent) = moved.source.parent() {
                                parents.push(parent);
                            }
                        }
                        AppliedChange::RetainedReceipt(moved) => {
                            if let Some(parent) = moved.source.parent() {
                                parents.push(parent);
                            }
                            if let Some(parent) = moved.parked.parent() {
                                parents.push(parent);
                            }
                        }
                        AppliedChange::Pruned(directory) => parents.push(directory),
                    }
                    for parent in parents {
                        if flushed.insert(parent.to_path_buf()) {
                            sync_directory(parent);
                        }
                    }
                }
                if let Err(error) = quarantine.close() {
                    log::warn!("could not delete the upgrade quarantine: {error}");
                }
                Ok(())
            }
            Err(error) => finish_upgrade(quarantine, &applied, &moved, error),
        }
    }
}

/// Undo a failed upgrade: remove the new paths, then restore the old ones.
///
/// The quarantine is kept when either side could not be fully undone, because
/// dropping it would delete paths still needed for manual recovery.
fn finish_upgrade(
    quarantine: tempfile::TempDir,
    applied: &[Applied],
    moved: &[AppliedChange],
    error: anyhow::Error,
) -> Result<()> {
    let mut issues = rollback(applied);
    issues.extend(rollback_removal(moved));
    if issues.is_empty() {
        drop(quarantine);
        Err(error)
    } else {
        let kept = quarantine.keep();
        Err(error.context(format!(
            "rollback was incomplete: {}; the quarantined old paths were kept in '{}'",
            issues.join("; "),
            kept.display()
        )))
    }
}

/// Move a preflighted active receipt into the retained records, rechecking
/// that it is still a regular receipt and that no retained record already
/// exists for that exact version.
///
/// The destination derives from the receipt's own validated name and version,
/// so the move stays inside `.brewrs/state/retained/<name>/`. The keg stays
/// in place; only the receipt file moves.
fn retain_receipt(
    prefix: &Path,
    name: &str,
    version: &str,
    source: &Path,
    applied: &mut Vec<AppliedChange>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to retain '{}': it disappeared after preflight",
            source.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", source.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing to retain '{}': it is no longer a regular receipt",
            source.display()
        );
    }

    // The retained root may not exist yet (an initialized prefix from before
    // retention shipped never had one), so both levels are created lazily;
    // `ensure_directory` creates exactly one level and never a symlink.
    ensure_directory(&retained_dir(prefix), true)?;
    let package_dir = retained_dir(prefix).join(name);
    ensure_directory(&package_dir, true)?;
    let parked = package_dir.join(format!("{version}.json"));
    match fs::symlink_metadata(&parked) {
        Ok(_) => bail!(
            "refusing to retain '{} {}': '{}' already exists; remove the stale retained record manually",
            name,
            version,
            parked.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", parked.display()));
        }
    }
    fs::rename(source, &parked).with_context(|| {
        format!(
            "failed to move the receipt of '{} {}' into the retained records",
            name, version
        )
    })?;
    sync_directory(&package_dir);
    sync_directory(&retained_dir(prefix));
    sync_directory(&state_dir(prefix));
    applied.push(AppliedChange::RetainedReceipt(MovedPath {
        source: source.to_path_buf(),
        parked,
    }));
    Ok(())
}
