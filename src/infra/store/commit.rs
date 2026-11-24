//! Commit, promotions, and rollback of a failed commit.

use std::collections::HashSet;
use std::fs;
use std::io;

use super::StoreError;
use super::error::error_chain;

use super::links::{create_symlink, keg_link_target, link_directory, opt_link_target};
use super::lock::InstallLock;
use super::paths::{cellar_dir, opt_dir, receipts_dir, sbin_dir};
use super::prefix::ensure_directory;
use super::receipt::{Prepared, Receipt, replace_receipt, write_receipt};
use super::validate::{
    validate_conflicts, validate_dependencies_with_retained, verify_retained_pin_kegs,
};
use super::{Applied, Pending, Promotion, Store};

impl Store {
    /// Install all prepared packages as one serialized transaction.
    ///
    /// The caller is expected to skip packages that are already installed at
    /// the same version; this method rejects them defensively because it cannot
    /// know whether the caller's view is still current. Under the writer lock
    /// the projected receipt graph is validated first, so every direct
    /// dependency of every receipt this commit would leave behind must exist at
    /// its exact name, version, and digest.
    pub(crate) fn commit(&self, packages: Vec<Prepared>) -> Result<(), StoreError> {
        self.commit_inner(packages, None)
    }

    pub(super) fn commit_inner(
        &self,
        packages: Vec<Prepared>,
        fail_after: Option<usize>,
    ) -> Result<(), StoreError> {
        self.ensure_owned()?;
        if packages.is_empty() {
            return Ok(());
        }
        // The lock is held for validation as well as application, so receipts
        // and links observed by preflight cannot change underneath us.
        let _lock = InstallLock::acquire(&self.prefix)?;
        let pending = self.prepare(packages)?;
        let mut applied = Vec::new();
        match self.apply(pending, fail_after, &mut applied) {
            Ok(()) => Ok(()),
            Err(error) => finish_commit(error, rollback(&applied)),
        }
    }

    /// Install all prepared packages and mark the exact installed receipts in
    /// `expected` as requested in one serialized transaction.
    ///
    /// This is [`Store::commit`] plus the promotion step an explicit install of
    /// an already installed dependency needs. Each receipt in `expected` must
    /// come from [`Store::installed`] and is re-read under the same writer lock:
    /// a receipt that is missing, changed, or already requested is refused, as
    /// is a duplicate promotion, a promotion whose name also appears in
    /// `packages`, and a package that does not own its real keg. Only the
    /// `requested` flag changes from `false` to `true`.
    ///
    /// New packages are applied exactly as [`Store::commit`] applies them, and
    /// the projected receipt graph (installed packages with the promotions
    /// applied, plus the new packages) is validated before anything is written.
    /// A failed install or promotion rolls back every new path and restores the
    /// exact bytes of every receipt this call already promoted.
    ///
    /// With an empty `expected`, this delegates to [`Store::commit`].
    pub(crate) fn commit_with_promotions(
        &self,
        packages: Vec<Prepared>,
        expected: &[Receipt],
    ) -> Result<(), StoreError> {
        if expected.is_empty() {
            return self.commit(packages);
        }
        self.commit_with_promotions_inner(packages, expected, None, None)
    }

    /// [`Store::commit_with_promotions`] with failure points used by the tests.
    ///
    /// `fail_install_after` fails before that install index, and
    /// `fail_promotion_after` fails before that promotion index after the
    /// preceding promotions were published.
    pub(super) fn commit_with_promotions_inner(
        &self,
        packages: Vec<Prepared>,
        expected: &[Receipt],
        fail_install_after: Option<usize>,
        fail_promotion_after: Option<usize>,
    ) -> Result<(), StoreError> {
        self.ensure_owned()?;
        // The lock covers promotion planning, install preflight, application,
        // and rollback, so no concurrent brewrs writer can change a receipt,
        // keg, link, or graph observed by this transaction.
        let _lock = InstallLock::acquire(&self.prefix)?;
        // Ownership may have changed while the lock was being acquired.
        self.ensure_owned()?;

        let pending_names: HashSet<&str> = packages
            .iter()
            .map(|package| package.receipt.name.as_str())
            .collect();
        let promotions = self.plan_promotions(expected, &pending_names)?;
        let pending = self.prepare(packages)?;

        // The complete graph this transaction would leave behind. Promotions
        // change only the requested flag, so they cannot alter a pin, but
        // validating the projected set keeps the mixed transaction honest.
        let mut projected = self.installed()?;
        for receipt in &mut projected {
            if let Some(promotion) = promotions
                .iter()
                .find(|promotion| promotion.promoted.name == receipt.name)
            {
                *receipt = promotion.promoted.clone();
            }
        }
        let newcomers: Vec<Receipt> = pending
            .iter()
            .map(|package| package.receipt.clone())
            .collect();
        let retained = self.retained()?;
        validate_dependencies_with_retained(&projected, &newcomers, &retained)?;
        let mut active = projected.clone();
        active.extend(newcomers.iter().cloned());
        verify_retained_pin_kegs(&projected, &active, &retained, &cellar_dir(&self.prefix))?;
        projected.extend(newcomers);
        validate_conflicts(&projected)?;

        // New packages are applied first: if they fail, nothing was promoted.
        let mut applied = Vec::new();
        if let Err(error) = self.apply(pending, fail_install_after, &mut applied) {
            return finish_commit(error, rollback(&applied));
        }

        let mut applied_promotions = Vec::new();
        if let Err(error) =
            apply_promotions(promotions, fail_promotion_after, &mut applied_promotions)
        {
            let mut issues = restore_promotions(&applied_promotions);
            issues.extend(rollback(&applied));
            return finish_commit(error, issues);
        }
        Ok(())
    }

    /// Apply validated packages, recording each owned path for rollback.
    fn apply(
        &self,
        pending: Vec<Pending>,
        fail_after: Option<usize>,
        applied: &mut Vec<Applied>,
    ) -> Result<(), StoreError> {
        self.apply_inner(pending, fail_after, applied, false)
    }

    /// Apply validated packages with an optional pre-existing package
    /// directory, which an upgrade leaves in place after moving its old keg.
    pub(super) fn apply_inner(
        &self,
        pending: Vec<Pending>,
        fail_after: Option<usize>,
        applied: &mut Vec<Applied>,
        reuse_cellar: bool,
    ) -> Result<(), StoreError> {
        let cellar = cellar_dir(&self.prefix);
        let sbin = sbin_dir(&self.prefix);
        let opt = opt_dir(&self.prefix);
        let receipts = receipts_dir(&self.prefix);
        ensure_directory(&sbin, true)?;
        ensure_directory(&opt, true)?;

        for (index, package) in pending.into_iter().enumerate() {
            if fail_after == Some(index) {
                return Err(StoreError::InjectedFailure {
                    what: "commit",
                    unit: "package",
                    index,
                });
            }
            let Pending {
                receipt,
                stage,
                links,
            } = package;
            let name = receipt.name.clone();
            let version = receipt.version.clone();

            let package_cellar = cellar.join(&name);
            // Preflight accepted a package directory that holds only retained
            // kegs; `create_dir` then fails with AlreadyExists and the
            // directory is reused. Anything else appearing here under the
            // writer lock fails closed.
            let mut cellar_created = !reuse_cellar;
            if cellar_created {
                match fs::create_dir(&package_cellar) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(&package_cellar)
                            .map_err(StoreError::io("failed to inspect", &package_cellar))?;
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            return Err(StoreError::CellarChanged {
                                name: name.to_string(),
                                path: package_cellar.to_path_buf(),
                            });
                        }
                        cellar_created = false;
                    }
                    Err(error) => {
                        return Err(StoreError::CreateCellar {
                            path: package_cellar.clone(),
                            name: name.clone(),
                            version: version.clone(),
                            source: error,
                        });
                    }
                }
            }
            let keg = package_cellar.join(&version);
            applied.push(Applied {
                cellar: package_cellar.clone(),
                cellar_created,
                keg: keg.clone(),
                keg_created: false,
                links: Vec::new(),
                link_dirs_created: Vec::new(),
                receipt: receipts.join(format!("{name}.json")),
            });

            fs::rename(stage.path(), &keg).map_err(|source| StoreError::MoveStage {
                name: name.clone(),
                version: version.clone(),
                path: keg.clone(),
                source,
            })?;

            let record = applied.last_mut().ok_or(StoreError::AppliedRecordMissing)?;
            record.keg_created = true;
            let opt_link_path = opt.join(&name);
            let opt_target = opt_link_target(&name, &version);
            create_symlink(&opt_target, &opt_link_path).map_err(StoreError::io(
                "failed to create the opt link",
                &opt_link_path,
            ))?;
            record.links.push(opt_link_path);
            for link in links {
                let Some(link_dir) = link_directory(
                    &self.prefix,
                    link.subdir,
                    Some(&mut record.link_dirs_created),
                )?
                else {
                    return Err(StoreError::LinkDirectoryUnavailable {
                        path: self.prefix.join(link.subdir).to_path_buf(),
                    });
                };
                let link_path = link_dir.join(&link.name);
                let target = keg_link_target(&name, &version, link.subdir, &link.name);
                create_symlink(&target, &link_path).map_err(StoreError::io(
                    "failed to create the managed link",
                    &link_path,
                ))?;
                record.links.push(link_path);
            }

            write_receipt(&receipts, &receipt)?;
        }
        Ok(())
    }
}

/// Remove every path created by a failed commit, newest package first.
///
/// Only paths this commit created are removed. A receipt that cannot be
/// removed is reported and its package is left untouched rather than exposing
/// a receipt whose files are gone.
pub(super) fn rollback(applied: &[Applied]) -> Vec<String> {
    let mut issues = Vec::new();
    for record in applied.iter().rev() {
        let receipt_removed = match fs::symlink_metadata(&record.receipt) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                issues.push(format!(
                    "left '{}': it is not a regular receipt",
                    record.receipt.display()
                ));
                false
            }
            Ok(_) => match fs::remove_file(&record.receipt) {
                Ok(()) => true,
                Err(error) => {
                    issues.push(format!(
                        "failed to remove '{}': {error}",
                        record.receipt.display()
                    ));
                    false
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => {
                issues.push(format!(
                    "failed to inspect '{}': {error}",
                    record.receipt.display()
                ));
                false
            }
        };
        if !receipt_removed {
            continue;
        }

        for link in record.links.iter().rev() {
            match fs::symlink_metadata(link) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if let Err(error) = fs::remove_file(link) {
                        issues.push(format!("failed to remove '{}': {error}", link.display()));
                    }
                }
                Ok(_) => issues.push(format!(
                    "left '{}': it is no longer a brewrs symlink",
                    link.display()
                )),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    issues.push(format!("failed to inspect '{}': {error}", link.display()));
                }
            }
        }

        for directory in record.link_dirs_created.iter().rev() {
            if let Err(error) = fs::remove_dir(directory)
                && error.kind() != io::ErrorKind::NotFound
            {
                issues.push(format!(
                    "failed to remove new link directory '{}': {error}",
                    directory.display()
                ));
            }
        }

        let removed = if record.cellar_created {
            Some((&record.cellar, fs::remove_dir_all(&record.cellar)))
        } else if record.keg_created {
            // The package directory belongs to the old install and is needed
            // for restoring its keg; only the new keg may be removed.
            Some((&record.keg, fs::remove_dir_all(&record.keg)))
        } else {
            None
        };
        if let Some((path, removed)) = removed {
            match removed {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    issues.push(format!("failed to remove '{}': {error}", path.display()))
                }
            }
        }
    }
    issues
}

/// Report a failed transaction, annotating it when rollback was incomplete.
fn finish_commit(error: StoreError, issues: Vec<String>) -> Result<(), StoreError> {
    if issues.is_empty() {
        Err(error)
    } else {
        Err(StoreError::CommitRollbackIncomplete {
            issues,
            source: Box::new(error),
        })
    }
}

/// Publish every preflighted promotion, recording each for rollback.
fn apply_promotions(
    promotions: Vec<Promotion>,
    fail_after: Option<usize>,
    applied: &mut Vec<Promotion>,
) -> Result<(), StoreError> {
    for (index, promotion) in promotions.into_iter().enumerate() {
        if fail_after == Some(index) {
            return Err(StoreError::InjectedFailure {
                what: "promotion",
                unit: "promotion",
                index,
            });
        }
        let current = fs::read(&promotion.path).map_err(|source| StoreError::PromotionReread {
            path: promotion.path.clone(),
            name: promotion.promoted.name.clone(),
            version: promotion.promoted.version.clone(),
            source,
        })?;
        if current != promotion.original {
            return Err(StoreError::PromotionRaced {
                name: promotion.promoted.name.to_string(),
                version: promotion.promoted.version.to_string(),
            });
        }
        replace_receipt(&promotion.path, &promotion.bytes).map_err(|source| {
            StoreError::PromotionWrite {
                name: promotion.promoted.name.clone(),
                version: promotion.promoted.version.clone(),
                source: Box::new(source),
            }
        })?;
        applied.push(promotion);
    }
    Ok(())
}

/// Restore the exact receipt bytes of every promotion this transaction
/// published, newest first.
///
/// A receipt that changed after its promotion is left alone and reported, so a
/// rollback never overwrites a receipt another writer published.
fn restore_promotions(applied: &[Promotion]) -> Vec<String> {
    let mut issues = Vec::new();
    for promotion in applied.iter().rev() {
        match fs::symlink_metadata(&promotion.path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                issues.push(format!(
                    "left '{}': it is no longer a regular receipt",
                    promotion.path.display()
                ));
                continue;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                issues.push(format!(
                    "left '{}': the receipt disappeared",
                    promotion.path.display()
                ));
                continue;
            }
            Err(error) => {
                issues.push(format!(
                    "failed to inspect '{}': {error}",
                    promotion.path.display()
                ));
                continue;
            }
        }
        match fs::read(&promotion.path) {
            Ok(current) if current == promotion.bytes => {}
            Ok(_) => {
                issues.push(format!(
                    "left '{}': the receipt changed after its promotion",
                    promotion.path.display()
                ));
                continue;
            }
            Err(error) => {
                issues.push(format!(
                    "failed to read '{}': {error}",
                    promotion.path.display()
                ));
                continue;
            }
        }
        if let Err(error) = replace_receipt(&promotion.path, &promotion.original) {
            issues.push(format!(
                "failed to restore '{}': {}",
                promotion.path.display(),
                error_chain(&error)
            ));
        }
    }
    issues
}
