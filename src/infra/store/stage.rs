//! Staging and batch preparation before a commit.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use super::StoreError;
use super::error::{Action, DirFault};

use super::links::{existing_link_directories, staged_links};
use super::paths::{cellar_dir, opt_dir, receipts_dir, stages_dir};
use super::prefix::check_optional_directory;
use super::receipt::{
    Prepared, Receipt, encode_receipt, existing_receipt, read_receipt_with_bytes, validate_receipt,
};
use super::validate::{
    validate_conflicts, validate_dependencies_with_retained, verify_retained_pin_kegs,
};
use super::{Pending, Promotion, Store};

impl Store {
    /// Create a private staging directory below this store.
    pub(crate) fn stage(&self) -> Result<tempfile::TempDir, StoreError> {
        self.ensure_owned()?;
        let stages = stages_dir(&self.prefix);
        tempfile::Builder::new()
            .prefix("stage-")
            .tempdir_in(&stages)
            .map_err(StoreError::io("failed to create a stage below", &stages))
    }

    /// Check the whole batch against existing state before writing anything.
    pub(super) fn prepare(&self, packages: Vec<Prepared>) -> Result<Vec<Pending>, StoreError> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let opt = opt_dir(&self.prefix);
        let link_dirs = existing_link_directories(&self.prefix)?;
        let has_opt = check_optional_directory(&opt)?;
        let retained = self.retained()?;
        let retained_by_name: HashMap<&str, HashMap<&str, &Receipt>> =
            retained.iter().fold(HashMap::new(), |mut by_name, record| {
                by_name
                    .entry(record.name.as_str())
                    .or_default()
                    .insert(record.version.as_str(), record);
                by_name
            });
        let mut names = HashSet::new();
        let mut link_names = HashSet::new();
        let mut pending = Vec::with_capacity(packages.len());

        for Prepared { receipt, stage } in packages {
            validate_receipt(&receipt)?;
            if !names.insert(receipt.name.clone()) {
                return Err(StoreError::DuplicatePackage {
                    action: Action::Install,
                    name: receipt.name.to_string(),
                });
            }
            self.validate_stage(stage.path())?;
            let links = if receipt.keg_only {
                Vec::new()
            } else {
                staged_links(stage.path())?
            };

            let receipt_path = receipts.join(format!("{}.json", receipt.name));
            if let Some(existing) = existing_receipt(&receipt_path)? {
                if existing.version == receipt.version {
                    return Err(StoreError::AlreadyInstalled {
                        name: receipt.name.to_string(),
                        version: receipt.version.to_string(),
                        prefix: self.prefix.to_path_buf(),
                    });
                }
                return Err(StoreError::AlreadyInstalledOtherVersion {
                    name: receipt.name.to_string(),
                    installed: existing.version.to_string(),
                    requested: receipt.version.to_string(),
                });
            }

            let installed_dir = cellar.join(&receipt.name);
            match fs::symlink_metadata(&installed_dir) {
                Ok(_) => self.accept_existing_cellar(
                    &installed_dir,
                    &receipt,
                    retained_by_name.get(receipt.name.as_str()),
                )?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(StoreError::io("failed to inspect", &installed_dir)(error));
                }
            }

            if has_opt {
                let existing_opt = opt.join(&receipt.name);
                match fs::symlink_metadata(&existing_opt) {
                    Ok(_) => {
                        return Err(StoreError::LinkExists {
                            name: receipt.name.to_string(),
                            path: existing_opt.to_path_buf(),
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(StoreError::io("failed to inspect", &existing_opt)(error));
                    }
                }
            }

            for link in &links {
                if !link_names.insert(link.clone()) {
                    return Err(StoreError::LinkClash {
                        action: Action::Install,
                        subdir: link.subdir,
                        name: link.name.to_string(),
                    });
                }
                if let Some(directory) = link_dirs.get(link.subdir) {
                    let existing = directory.join(&link.name);
                    match fs::symlink_metadata(&existing) {
                        Ok(_) => {
                            return Err(StoreError::LinkExists {
                                name: receipt.name.to_string(),
                                path: existing.to_path_buf(),
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(StoreError::io("failed to inspect", &existing)(error));
                        }
                    }
                }
            }

            pending.push(Pending {
                receipt,
                stage,
                links,
            });
        }

        // Full preflight of the graph this commit would leave behind. A
        // dependency may name a package from this same batch, but its exact
        // version and digest must match, and a pin recorded by a *new* receipt
        // is satisfied only by an active receipt, never by a retained keg.
        // Receipts already on disk may pin a retained keg, and every such pin
        // must still own a real keg directory.
        let survivors = self.installed()?;
        let newcomers: Vec<Receipt> = pending
            .iter()
            .map(|package| package.receipt.clone())
            .collect();
        validate_dependencies_with_retained(&survivors, &newcomers, &retained)?;
        let mut final_receipts = survivors.clone();
        final_receipts.extend(newcomers.iter().cloned());
        verify_retained_pin_kegs(&survivors, &final_receipts, &retained, &cellar)?;
        validate_conflicts(&final_receipts)?;
        Ok(pending)
    }

    /// A package directory without an active receipt may exist only when every
    /// entry is a retained keg backed by a valid retained record.
    ///
    /// Any other entry is foreign and fails closed: installing must never
    /// adopt an unreceipted keg, and recreating a version that is still
    /// retained is refused explicitly rather than rebinding it.
    fn accept_existing_cellar(
        &self,
        directory: &Path,
        receipt: &Receipt,
        retained: Option<&HashMap<&str, &Receipt>>,
    ) -> Result<(), StoreError> {
        let metadata = fs::symlink_metadata(directory)
            .map_err(StoreError::io("failed to inspect", &directory))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::UnreceiptedPackageDirectory {
                name: receipt.name.to_string(),
                path: directory.to_path_buf(),
            });
        }
        for entry in
            fs::read_dir(directory).map_err(StoreError::io("failed to read", &directory))?
        {
            let entry = entry.map_err(StoreError::io("failed to read an entry in", &directory))?;
            let Some(version) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(StoreError::ForeignCellarEntry {
                    name: receipt.name.to_string(),
                    path: directory.to_path_buf(),
                });
            };
            if version == receipt.version {
                return Err(StoreError::VersionRetained {
                    name: receipt.name.to_string(),
                    version: receipt.version.to_string(),
                    path: entry.path().to_path_buf(),
                });
            }
            let Some(record) = retained.and_then(|versions| versions.get(version.as_str())) else {
                return Err(StoreError::UnrecordedCellarEntry {
                    name: receipt.name.to_string(),
                    directory: directory.to_path_buf(),
                    entry: version.to_string(),
                });
            };
            if record.name != receipt.name || record.version != version {
                return Err(StoreError::RetainedRecordMismatch {
                    action: Action::Install,
                    name: receipt.name.to_string(),
                    version: version.to_string(),
                    record_name: record.name.to_string(),
                    record_version: record.version.to_string(),
                });
            }
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(StoreError::io("failed to inspect", &entry.path()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(StoreError::RetainedKegNotDirectory {
                    action: Action::Install,
                    name: receipt.name.to_string(),
                    path: entry.path().to_path_buf(),
                });
            }
        }
        Ok(())
    }

    /// Recheck every promotion against the current receipts and kegs.
    ///
    /// Runs under the writer lock. A promotion may only flip `requested` from
    /// false to true on the exact receipt the caller observed, so any stale or
    /// changed receipt is refused before a single path is written.
    pub(super) fn plan_promotions(
        &self,
        expected: &[Receipt],
        pending_names: &HashSet<&str>,
    ) -> Result<Vec<Promotion>, StoreError> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let mut names = HashSet::new();
        let mut promotions = Vec::with_capacity(expected.len());

        for receipt in expected {
            validate_receipt(receipt)?;
            if !names.insert(receipt.name.clone()) {
                return Err(StoreError::DuplicatePackage {
                    action: Action::Promote,
                    name: receipt.name.to_string(),
                });
            }
            if pending_names.contains(receipt.name.as_str()) {
                return Err(StoreError::InstallAndPromote {
                    name: receipt.name.to_string(),
                });
            }
            if receipt.requested {
                return Err(StoreError::AlreadyRequested {
                    name: receipt.name.to_string(),
                    version: receipt.version.to_string(),
                });
            }

            let path = receipts.join(format!("{}.json", receipt.name));
            let (current, original) = read_receipt_with_bytes(&path).map_err(|source| {
                StoreError::PromotionReceiptUnreadable {
                    name: receipt.name.clone(),
                    version: receipt.version.clone(),
                    path: path.clone(),
                    source: Box::new(source),
                }
            })?;
            if current != *receipt {
                return Err(StoreError::PromotionReceiptChanged {
                    name: receipt.name.to_string(),
                    version: receipt.version.to_string(),
                    current_name: current.name.to_string(),
                    current_version: current.version.to_string(),
                    requested: current.requested,
                });
            }
            validate_promotion_keg(&receipt.name, &receipt.version, &cellar)?;

            let mut promoted = receipt.clone();
            promoted.requested = true;
            let bytes = encode_receipt(&promoted)?;
            promotions.push(Promotion {
                promoted,
                path,
                original,
                bytes,
            });
        }
        Ok(promotions)
    }

    /// A stage must be a real directory directly inside this store's stages.
    pub(super) fn validate_stage(&self, stage: &Path) -> Result<(), StoreError> {
        let stages = stages_dir(&self.prefix);
        let expected = stages
            .canonicalize()
            .map_err(StoreError::io("failed to resolve", &stages))?;
        let metadata = fs::symlink_metadata(stage)
            .map_err(StoreError::io("failed to inspect stage", &stage))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::StageNotDirectory {
                stage: stage.to_path_buf(),
            });
        }
        let actual = stage
            .canonicalize()
            .map_err(StoreError::io("failed to resolve stage", &stage))?;
        if actual.parent() != Some(expected.as_path()) {
            return Err(StoreError::StageNotOwned {
                stage: stage.to_path_buf(),
                stages: stages.to_path_buf(),
            });
        }
        Ok(())
    }
}

/// A promotion changes only a receipt, so the package must still own its real
/// keg: a symlinked package directory or keg is a foreign path and is refused.
fn validate_promotion_keg(name: &str, version: &str, cellar: &Path) -> Result<(), StoreError> {
    let package = cellar.join(name);
    match fs::symlink_metadata(&package) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(StoreError::PackageDirectory {
                action: Action::Promote,
                name: name.to_string(),
                version: version.to_string(),
                path: package.to_path_buf(),
                fault: DirFault::Symlink,
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(StoreError::PackageDirectory {
                action: Action::Promote,
                name: name.to_string(),
                version: version.to_string(),
                path: package.to_path_buf(),
                fault: DirFault::NotDirectory,
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(StoreError::PackageDirectory {
                action: Action::Promote,
                name: name.to_string(),
                version: version.to_string(),
                path: package.to_path_buf(),
                fault: DirFault::Missing,
            });
        }
        Err(error) => {
            return Err(StoreError::io("failed to inspect", &package)(error));
        }
    }

    let keg = package.join(version);
    match fs::symlink_metadata(&keg) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::Keg {
            action: Action::Promote,
            name: name.to_string(),
            version: version.to_string(),
            path: keg.to_path_buf(),
            fault: DirFault::Symlink,
        }),
        Ok(metadata) if !metadata.is_dir() => Err(StoreError::Keg {
            action: Action::Promote,
            name: name.to_string(),
            version: version.to_string(),
            path: keg.to_path_buf(),
            fault: DirFault::NotDirectory,
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(StoreError::Keg {
            action: Action::Promote,
            name: name.to_string(),
            version: version.to_string(),
            path: keg.to_path_buf(),
            fault: DirFault::Missing,
        }),
        Err(error) => Err(StoreError::io("failed to inspect", &keg)(error)),
    }
}
