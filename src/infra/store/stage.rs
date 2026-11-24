//! Staging and batch preparation before a commit.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

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
    pub(crate) fn stage(&self) -> Result<tempfile::TempDir> {
        self.ensure_owned()?;
        let stages = stages_dir(&self.prefix);
        tempfile::Builder::new()
            .prefix("stage-")
            .tempdir_in(&stages)
            .with_context(|| format!("failed to create a stage below '{}'", stages.display()))
    }

    /// Check the whole batch against existing state before writing anything.
    pub(super) fn prepare(&self, packages: Vec<Prepared>) -> Result<Vec<Pending>> {
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
                bail!(
                    "package '{}' appears more than once in one commit",
                    receipt.name
                );
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
                    bail!(
                        "'{} {}' is already installed in '{}'; the caller must skip it",
                        receipt.name,
                        receipt.version,
                        self.prefix.display()
                    );
                }
                bail!(
                    "'{}' is already installed at version {}; refusing to replace it with {}",
                    receipt.name,
                    existing.version,
                    receipt.version
                );
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
                    return Err(error).with_context(|| {
                        format!("failed to inspect '{}'", installed_dir.display())
                    });
                }
            }

            if has_opt {
                let existing_opt = opt.join(&receipt.name);
                match fs::symlink_metadata(&existing_opt) {
                    Ok(_) => bail!(
                        "refusing to install '{}': '{}' already exists; links are never overwritten",
                        receipt.name,
                        existing_opt.display()
                    ),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", existing_opt.display())
                        });
                    }
                }
            }

            for link in &links {
                if !link_names.insert(link.clone()) {
                    bail!(
                        "more than one package in this commit provides '{}/{}'",
                        link.subdir,
                        link.name
                    );
                }
                if let Some(directory) = link_dirs.get(link.subdir) {
                    let existing = directory.join(&link.name);
                    match fs::symlink_metadata(&existing) {
                        Ok(_) => bail!(
                            "refusing to install '{}': '{}' already exists; links are never overwritten",
                            receipt.name,
                            existing.display()
                        ),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("failed to inspect '{}'", existing.display())
                            });
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
    ) -> Result<()> {
        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("failed to inspect '{}'", directory.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing to install '{}': '{}' already exists without a receipt; remove it or use a clean prefix",
                receipt.name,
                directory.display()
            );
        }
        for entry in fs::read_dir(directory)
            .with_context(|| format!("failed to read '{}'", directory.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
            let Some(version) = entry.file_name().to_str().map(str::to_owned) else {
                bail!(
                    "refusing to install '{}': '{}' contains a foreign entry; remove it or use a clean prefix",
                    receipt.name,
                    directory.display()
                );
            };
            if version == receipt.version {
                bail!(
                    "refusing to install '{} {}': '{}' is retained by surviving receipts; install a different version or clean up the retained keg manually",
                    receipt.name,
                    receipt.version,
                    entry.path().display()
                );
            }
            let Some(record) = retained.and_then(|versions| versions.get(version.as_str())) else {
                bail!(
                    "refusing to install '{}': '{}' contains '{}', which has no retained receipt; remove foreign entries first",
                    receipt.name,
                    directory.display(),
                    version
                );
            };
            if record.name != receipt.name || record.version != version {
                bail!(
                    "refusing to install '{}': the retained record for '{}' names '{} {}'; remove it manually",
                    receipt.name,
                    version,
                    record.name,
                    record.version
                );
            }
            let metadata = fs::symlink_metadata(entry.path())
                .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "refusing to install '{}': retained keg '{}' is not a real directory",
                    receipt.name,
                    entry.path().display()
                );
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
    ) -> Result<Vec<Promotion>> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let mut names = HashSet::new();
        let mut promotions = Vec::with_capacity(expected.len());

        for receipt in expected {
            validate_receipt(receipt)?;
            if !names.insert(receipt.name.clone()) {
                bail!(
                    "package '{}' appears more than once in one promotion",
                    receipt.name
                );
            }
            if pending_names.contains(receipt.name.as_str()) {
                bail!(
                    "refusing to install and promote '{}' in the same call",
                    receipt.name
                );
            }
            if receipt.requested {
                bail!(
                    "refusing to promote '{} {}': the observed receipt is already requested",
                    receipt.name,
                    receipt.version
                );
            }

            let path = receipts.join(format!("{}.json", receipt.name));
            let (current, original) = read_receipt_with_bytes(&path).with_context(|| {
                format!(
                    "refusing to promote '{} {}': its receipt '{}' could not be re-read",
                    receipt.name,
                    receipt.version,
                    path.display()
                )
            })?;
            if current != *receipt {
                bail!(
                    "refusing to promote '{} {}': the installed receipt now records {} {} with requested={}; re-read the installed packages and try again",
                    receipt.name,
                    receipt.version,
                    current.name,
                    current.version,
                    current.requested
                );
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
    pub(super) fn validate_stage(&self, stage: &Path) -> Result<()> {
        let stages = stages_dir(&self.prefix);
        let expected = stages
            .canonicalize()
            .with_context(|| format!("failed to resolve '{}'", stages.display()))?;
        let metadata = fs::symlink_metadata(stage)
            .with_context(|| format!("failed to inspect stage '{}'", stage.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing stage '{}': it is not a real directory",
                stage.display()
            );
        }
        let actual = stage
            .canonicalize()
            .with_context(|| format!("failed to resolve stage '{}'", stage.display()))?;
        if actual.parent() != Some(expected.as_path()) {
            bail!(
                "refusing stage '{}': it is not owned by '{}'",
                stage.display(),
                stages.display()
            );
        }
        Ok(())
    }
}

/// A promotion changes only a receipt, so the package must still own its real
/// keg: a symlinked package directory or keg is a foreign path and is refused.
fn validate_promotion_keg(name: &str, version: &str, cellar: &Path) -> Result<()> {
    let package = cellar.join(name);
    match fs::symlink_metadata(&package) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to promote '{name} {version}': '{}' is a symlink",
            package.display()
        ),
        Ok(metadata) if !metadata.is_dir() => bail!(
            "refusing to promote '{name} {version}': '{}' is not a directory",
            package.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to promote '{name} {version}': '{}' is missing; remove it manually",
            package.display()
        ),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", package.display()));
        }
    }

    let keg = package.join(version);
    match fs::symlink_metadata(&keg) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to promote '{name} {version}': its keg '{}' is a symlink",
            keg.display()
        ),
        Ok(metadata) if !metadata.is_dir() => bail!(
            "refusing to promote '{name} {version}': its keg '{}' is not a directory",
            keg.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to promote '{name} {version}': its keg '{}' is missing; remove it manually",
            keg.display()
        ),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", keg.display())),
    }
}
