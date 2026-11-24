//! Removal planning, quarantine, and rollback.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::links::{
    LINK_DIRECTORIES, collect_owned_links, existing_link_directories, opt_link_target,
};
use super::lock::InstallLock;
use super::paths::{cellar_dir, opt_dir, receipts_dir, stages_dir, sync_directory};
use super::prefix::check_optional_directory;
use super::receipt::{Receipt, existing_receipt, validate_receipt};
use super::validate::{
    validate_dependencies_with_retained, validate_retained_pin_closure, verify_retained_pin_kegs,
};
use super::{AppliedChange, MovedPath, OwnedLink, Removal, RemovalStep, StepKind, Store};

impl Store {
    /// Remove exactly the packages named by `expected`.
    ///
    /// `expected` must come from a previous [`Store::installed`] call. Every
    /// receipt is rechecked under the same writer lock [`Store::commit`] uses,
    /// so a stale selection (a different version, digest, or requested flag, or
    /// a receipt that is already gone) is rejected before any path moves. The
    /// caller may hold a read-only binding: removal never initializes a prefix.
    ///
    /// The projected receipt graph is validated as well: a receipt that remains
    /// installed and depends on a package this call removes blocks the removal
    /// with an error naming both packages. Selecting the dependent in the same
    /// call allows the batch.
    ///
    /// It removes the receipt, the receipt-owned keg, and only the `bin` links
    /// whose stored target is exactly the relative link [`Store::commit`]
    /// created for that package and version, including a link left dangling by
    /// a deleted keg member or stale by a non-executable one. Foreign files,
    /// directories, and links are preserved and never followed.
    pub(crate) fn remove(&self, expected: &[Receipt]) -> Result<()> {
        self.remove_inner(expected, None)
    }

    /// [`Store::remove`] with a failure point used by the tests.
    pub(super) fn remove_inner(
        &self,
        expected: &[Receipt],
        fail_after: Option<usize>,
    ) -> Result<()> {
        self.verify_owned()?;
        if expected.is_empty() {
            return Ok(());
        }
        // The lock covers the recheck and every move, so receipts, links, and
        // kegs cannot change underneath a concurrent brewrs writer.
        let _lock = InstallLock::acquire(&self.prefix)?;
        // Ownership may have changed while the lock was being acquired.
        self.verify_owned()?;
        let removals = self.plan_removal(expected)?;
        self.apply_removal(removals, fail_after)
    }

    /// Check every selected receipt, keg, and CLI link before moving anything.
    fn plan_removal(&self, expected: &[Receipt]) -> Result<Vec<Removal>> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let opt = opt_dir(&self.prefix);
        let link_dirs = existing_link_directories(&self.prefix)?;
        let has_opt = check_optional_directory(&opt)?;
        let mut names = HashSet::new();
        let mut links_seen = HashSet::new();
        let mut removals = Vec::with_capacity(expected.len());

        for receipt in expected {
            validate_receipt(receipt)?;
            if !names.insert(receipt.name.clone()) {
                bail!(
                    "package '{}' appears more than once in one removal",
                    receipt.name
                );
            }

            let receipt_path = receipts.join(format!("{}.json", receipt.name));
            let current = existing_receipt(&receipt_path)?.with_context(|| {
                format!(
                    "refusing to remove '{} {}': its receipt '{}' no longer exists",
                    receipt.name,
                    receipt.version,
                    receipt_path.display()
                )
            })?;
            if current != *receipt {
                bail!(
                    "refusing to remove '{} {}': the installed receipt now records {} {}; re-read the installed packages and try again",
                    receipt.name,
                    receipt.version,
                    current.name,
                    current.version
                );
            }

            let package = cellar.join(&receipt.name);
            match fs::symlink_metadata(&package) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to remove '{}': '{}' is a symlink",
                    receipt.name,
                    package.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to remove '{}': '{}' is not a directory",
                    receipt.name,
                    package.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to remove '{} {}': '{}' is missing; remove it manually",
                    receipt.name,
                    receipt.version,
                    package.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", package.display()));
                }
            }

            let keg = package.join(&receipt.version);
            match fs::symlink_metadata(&keg) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to remove '{} {}': '{}' is a symlink",
                    receipt.name,
                    receipt.version,
                    keg.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to remove '{} {}': '{}' is not a directory",
                    receipt.name,
                    receipt.version,
                    keg.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to remove '{} {}': its keg '{}' is missing; remove it manually",
                    receipt.name,
                    receipt.version,
                    keg.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", keg.display()));
                }
            }

            // Scan the prefix's own managed link directories. Deriving names
            // from the keg would miss an owned link whose keg member was
            // deleted or made non-executable, and a reinstall would then
            // collide with it. Only an exact stored target identifies a brewrs
            // link; everything else is foreign and is preserved.
            let mut links = Vec::new();
            for root in LINK_DIRECTORIES {
                if let Some(directory) = link_dirs.get(root.subdir) {
                    links.extend(collect_owned_links(
                        directory,
                        &root,
                        &receipt.name,
                        &receipt.version,
                    )?);
                }
            }
            for link in &links {
                if !links_seen.insert(link.path.clone()) {
                    bail!(
                        "refusing to remove '{}': more than one selected package claims the link '{}'",
                        receipt.name,
                        link.path.display()
                    );
                }
            }
            if has_opt {
                let opt_path = opt.join(&receipt.name);
                let opt_target = opt_link_target(&receipt.name, &receipt.version);
                match fs::symlink_metadata(&opt_path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let actual = fs::read_link(&opt_path).with_context(|| {
                            format!("failed to read link '{}'", opt_path.display())
                        })?;
                        if actual.as_os_str() == opt_target.as_os_str() {
                            if !links_seen.insert(opt_path.clone()) {
                                bail!(
                                    "refusing to remove '{}': more than one selected package claims the link '{}'",
                                    receipt.name,
                                    opt_path.display()
                                );
                            }
                            links.push(OwnedLink {
                                path: opt_path,
                                target: opt_target,
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", opt_path.display())
                        });
                    }
                }
            }
            links.sort_by(|left, right| left.path.cmp(&right.path));

            removals.push(Removal {
                receipt_path,
                package,
                keg,
                links,
            });
        }

        // A removal must not strand a dependent that stays installed. The graph
        // this call would leave behind is the installed set minus the selection;
        // `installed` runs under the same lock as the moves below, so the check
        // sees exactly the receipts those moves will delete. A remaining pin may
        // be satisfied by a retained keg, which removal never touches, and every
        // such pin must still own a real keg directory.
        let selected: HashSet<&str> = expected
            .iter()
            .map(|receipt| receipt.name.as_str())
            .collect();
        let remaining: Vec<Receipt> = self
            .installed()?
            .into_iter()
            .filter(|receipt| !selected.contains(receipt.name.as_str()))
            .collect();
        let retained = self.retained()?;
        validate_dependencies_with_retained(&remaining, &[], &retained)?;
        validate_retained_pin_closure(&retained, &remaining, &retained)?;
        let mut pin_owners = remaining.clone();
        pin_owners.extend(retained.iter().cloned());
        verify_retained_pin_kegs(&pin_owners, &remaining, &retained, &cellar)?;
        Ok(removals)
    }

    /// Move preflighted paths into a private quarantine, rolling back on error.
    fn apply_removal(&self, removals: Vec<Removal>, fail_after: Option<usize>) -> Result<()> {
        let stages = stages_dir(&self.prefix);
        let quarantine = tempfile::Builder::new()
            .prefix("remove-")
            .tempdir_in(&stages)
            .with_context(|| {
                format!(
                    "failed to create a removal quarantine below '{}'",
                    stages.display()
                )
            })?;

        let mut steps = Vec::new();
        for removal in removals {
            steps.push(RemovalStep {
                source: removal.receipt_path,
                kind: StepKind::Receipt,
            });
            for link in removal.links {
                steps.push(RemovalStep {
                    source: link.path,
                    kind: StepKind::Link {
                        target: link.target,
                    },
                });
            }
            steps.push(RemovalStep {
                source: removal.keg,
                kind: StepKind::Directory,
            });
            // Move only the exact version directory: the package directory may
            // gain foreign entries between preflight and this rename. Once the
            // keg is parked, an empty package directory is pruned with a
            // non-recursive removal that can never delete a foreign entry.
            steps.push(RemovalStep {
                source: removal.package,
                kind: StepKind::Prune,
            });
        }

        let mut applied = Vec::new();
        let outcome = (|| -> Result<()> {
            for (index, step) in steps.iter().enumerate() {
                if fail_after == Some(index) {
                    bail!("injected removal failure before move {index}");
                }
                if matches!(&step.kind, StepKind::Prune) {
                    prune_package_dir(step, &mut applied)?;
                } else {
                    move_aside(step, quarantine.path(), index, &mut applied)?;
                }
            }
            Ok(())
        })();

        match outcome {
            Ok(()) => {
                let mut flushed = HashSet::new();
                for change in &applied {
                    let source = match change {
                        AppliedChange::Moved(moved) => &moved.source,
                        AppliedChange::RetainedReceipt(_) => continue,
                        AppliedChange::Pruned(directory) => directory,
                    };
                    if let Some(parent) = source.parent()
                        && flushed.insert(parent.to_path_buf())
                    {
                        sync_directory(parent);
                    }
                }
                if let Err(error) = quarantine.close() {
                    log::warn!("could not delete the removal quarantine: {error}");
                }
                Ok(())
            }
            Err(error) => {
                let issues = rollback_removal(&applied);
                if issues.is_empty() {
                    drop(quarantine);
                    Err(error)
                } else {
                    // Keep the quarantine: dropping it would delete paths that
                    // are still needed for a manual rollback.
                    let kept = quarantine.keep();
                    Err(error.context(format!(
                        "rollback was incomplete: {}; the quarantined paths were kept in '{}'",
                        issues.join("; "),
                        kept.display()
                    )))
                }
            }
        }
    }
}

/// Move one preflighted path into the quarantine, rechecking its exact type.
///
/// Renaming a symlink moves the link itself, so this never follows a foreign
/// target. A path that changed since preflight aborts the whole removal.
pub(super) fn move_aside(
    step: &RemovalStep,
    quarantine: &Path,
    index: usize,
    applied: &mut Vec<AppliedChange>,
) -> Result<()> {
    let source = &step.source;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to remove '{}': it disappeared after preflight",
            source.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", source.display()));
        }
    };
    match &step.kind {
        StepKind::Receipt if metadata.file_type().is_symlink() || !metadata.is_file() => bail!(
            "refusing to remove '{}': it is no longer a regular receipt",
            source.display()
        ),
        StepKind::Directory if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing to remove '{}': it is no longer a real directory",
            source.display()
        ),
        StepKind::Link { target } => {
            if !metadata.file_type().is_symlink() {
                bail!(
                    "refusing to remove '{}': it is no longer a brewrs symlink",
                    source.display()
                );
            }
            let actual = fs::read_link(source)
                .with_context(|| format!("failed to read link '{}'", source.display()))?;
            if actual.as_os_str() != target.as_os_str() {
                bail!(
                    "refusing to remove '{}': the link target is now '{}'",
                    source.display(),
                    actual.display()
                );
            }
        }
        _ => {}
    }

    let parked = quarantine.join(format!("item-{index}"));
    fs::rename(source, &parked).with_context(|| {
        format!(
            "failed to move '{}' into the removal quarantine",
            source.display()
        )
    })?;
    applied.push(AppliedChange::Moved(MovedPath {
        source: source.clone(),
        parked,
    }));
    Ok(())
}

/// Remove the package directory, but only while it is empty.
///
/// A non-recursive removal cannot delete a foreign entry that appeared after
/// preflight: it fails with `DirectoryNotEmpty` and the directory is kept.
fn prune_package_dir(step: &RemovalStep, applied: &mut Vec<AppliedChange>) -> Result<()> {
    let directory = &step.source;
    match fs::remove_dir(directory) {
        Ok(()) => {
            applied.push(AppliedChange::Pruned(directory.clone()));
            Ok(())
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove the emptied package directory '{}'",
                directory.display()
            )
        }),
    }
}

/// Undo every applied removal, newest change first.
pub(super) fn rollback_removal(applied: &[AppliedChange]) -> Vec<String> {
    let mut issues = Vec::new();
    for change in applied.iter().rev() {
        match change {
            AppliedChange::Pruned(directory) => match fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => issues.push(format!(
                    "failed to recreate '{}': {error}",
                    directory.display()
                )),
            },
            AppliedChange::Moved(moved) => {
                match fs::symlink_metadata(&moved.source) {
                    Ok(_) => {
                        issues.push(format!(
                            "left '{}' in quarantine: the path reappeared",
                            moved.source.display()
                        ));
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        issues.push(format!(
                            "failed to inspect '{}': {error}",
                            moved.source.display()
                        ));
                        continue;
                    }
                }
                if let Err(error) = fs::rename(&moved.parked, &moved.source) {
                    issues.push(format!(
                        "failed to restore '{}': {error}",
                        moved.source.display()
                    ));
                }
            }
            AppliedChange::RetainedReceipt(moved) => {
                // The new receipt is removed before this runs, so the active
                // slot is expected to be free; anything else is never
                // overwritten and the retained record is left in place.
                match fs::symlink_metadata(&moved.source) {
                    Ok(_) => {
                        issues.push(format!(
                            "left the retained record '{}' in place: '{}' still exists",
                            moved.parked.display(),
                            moved.source.display()
                        ));
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        issues.push(format!(
                            "failed to inspect '{}': {error}",
                            moved.source.display()
                        ));
                        continue;
                    }
                }
                if let Err(error) = fs::rename(&moved.parked, &moved.source) {
                    issues.push(format!(
                        "failed to restore the retained receipt '{}': {error}",
                        moved.source.display()
                    ));
                }
            }
        }
    }
    issues
}
