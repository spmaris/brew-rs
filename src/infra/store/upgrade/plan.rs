//! Upgrade preflight.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;

use anyhow::{Context, Result, bail};

use super::super::links::{
    LINK_DIRECTORIES, collect_owned_links, existing_link_directories, opt_link_target, staged_links,
};
use super::super::paths::{cellar_dir, opt_dir, receipts_dir};
use super::super::prefix::check_optional_directory;
use super::super::receipt::{Prepared, Receipt, existing_receipt, validate_receipt};
use super::super::validate::{
    validate_conflicts, validate_dependencies_with_retained, validate_retained_pin_closure,
    verify_retained_pin_kegs,
};
use super::super::{OwnedLink, Pending, Removal, Store, Upgrade};

impl Store {
    /// Check every old receipt, keg, and owned link and every replacement
    /// before moving anything.
    pub(super) fn plan_upgrade(&self, packages: Vec<(Receipt, Prepared)>) -> Result<Vec<Upgrade>> {
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
        let mut upgrades = Vec::with_capacity(packages.len());

        for (old, prepared) in packages {
            let Prepared { receipt, stage } = prepared;
            validate_receipt(&old)?;
            validate_receipt(&receipt)?;
            if old.name != receipt.name {
                bail!(
                    "refusing to upgrade '{}': the staged package is '{}'",
                    old.name,
                    receipt.name
                );
            }
            if !names.insert(old.name.clone()) {
                bail!(
                    "package '{}' appears more than once in one upgrade",
                    old.name
                );
            }
            if old.requested != receipt.requested {
                bail!(
                    "refusing to upgrade '{} {}': the replacement would change its requested status",
                    old.name,
                    old.version
                );
            }
            if old.version == receipt.version && old.version_scheme == receipt.version_scheme {
                bail!(
                    "refusing to upgrade '{}': version {} is already installed; the caller must skip it",
                    old.name,
                    old.version
                );
            }
            self.validate_stage(stage.path())?;
            let links = if receipt.keg_only {
                Vec::new()
            } else {
                staged_links(stage.path())?
            };

            let receipt_path = receipts.join(format!("{}.json", old.name));
            let current = existing_receipt(&receipt_path)?.with_context(|| {
                format!(
                    "refusing to upgrade '{} {}': its receipt '{}' no longer exists",
                    old.name,
                    old.version,
                    receipt_path.display()
                )
            })?;
            if current != old {
                bail!(
                    "refusing to upgrade '{} {}': the installed receipt now records {} {}; re-read the installed packages and try again",
                    old.name,
                    old.version,
                    current.name,
                    current.version
                );
            }

            let package = cellar.join(&old.name);
            match fs::symlink_metadata(&package) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to upgrade '{}': '{}' is a symlink",
                    old.name,
                    package.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to upgrade '{}': '{}' is not a directory",
                    old.name,
                    package.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to upgrade '{} {}': '{}' is missing; remove it manually",
                    old.name,
                    old.version,
                    package.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", package.display()));
                }
            }

            let keg = package.join(&old.version);
            match fs::symlink_metadata(&keg) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to upgrade '{} {}': '{}' is a symlink",
                    old.name,
                    old.version,
                    keg.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to upgrade '{} {}': '{}' is not a directory",
                    old.name,
                    old.version,
                    keg.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to upgrade '{} {}': its keg '{}' is missing; remove it manually",
                    old.name,
                    old.version,
                    keg.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", keg.display()));
                }
            }

            // The package directory may hold the receipt's keg plus any kegs
            // with valid retained records. An entry with no retained record, a
            // version that is both active and retained, or a retained keg that
            // is not a real directory fails closed: only the active keg and
            // recorded retained kegs may survive this upgrade.
            if let Some(versions) = retained_by_name.get(old.name.as_str())
                && versions.contains_key(old.version.as_str())
            {
                bail!(
                    "refusing to upgrade '{}': the active version {} also has a retained record; remove the stale retained record manually",
                    old.name,
                    old.version
                );
            }
            for entry in fs::read_dir(&package)
                .with_context(|| format!("failed to read '{}'", package.display()))?
            {
                let entry = entry.with_context(|| {
                    format!("failed to read an entry in '{}'", package.display())
                })?;
                let file_name = entry.file_name();
                if file_name.as_os_str() == OsStr::new(&old.version) {
                    continue;
                }
                let Some(version) = file_name.to_str() else {
                    bail!(
                        "refusing to upgrade '{}': '{}' contains a foreign entry; remove it first",
                        old.name,
                        package.display()
                    );
                };
                let Some(record) = retained_by_name
                    .get(old.name.as_str())
                    .and_then(|versions| versions.get(version))
                else {
                    bail!(
                        "refusing to upgrade '{}': '{}' contains '{}', which is not the installed version {} and has no retained record; remove foreign entries first",
                        old.name,
                        package.display(),
                        file_name.to_string_lossy(),
                        old.version
                    );
                };
                if record.name != old.name || record.version != version {
                    bail!(
                        "refusing to upgrade '{}': the retained record for '{}' names '{} {}'; remove it manually",
                        old.name,
                        version,
                        record.name,
                        record.version
                    );
                }
                let metadata = fs::symlink_metadata(entry.path())
                    .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "refusing to upgrade '{}': retained keg '{}' is not a real directory",
                        old.name,
                        entry.path().display()
                    );
                }
            }

            let mut owned_links = Vec::new();
            for root in LINK_DIRECTORIES {
                if let Some(directory) = link_dirs.get(root.subdir) {
                    owned_links.extend(collect_owned_links(
                        directory,
                        &root,
                        &old.name,
                        &old.version,
                    )?);
                }
            }
            if has_opt {
                let opt_path = opt.join(&old.name);
                let opt_target = opt_link_target(&old.name, &old.version);
                match fs::symlink_metadata(&opt_path) {
                    Ok(metadata) => {
                        let owned = if metadata.file_type().is_symlink() {
                            let actual = fs::read_link(&opt_path).with_context(|| {
                                format!("failed to read link '{}'", opt_path.display())
                            })?;
                            actual.as_os_str() == opt_target.as_os_str()
                        } else {
                            false
                        };
                        if owned {
                            owned_links.push(OwnedLink {
                                path: opt_path,
                                target: opt_target,
                            });
                        } else {
                            bail!(
                                "refusing to upgrade '{}': '{}' already exists and is not a link owned by {} {}; refusing to overwrite it",
                                old.name,
                                opt_path.display(),
                                old.name,
                                old.version
                            );
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", opt_path.display())
                        });
                    }
                }
            }
            owned_links.sort_by(|left, right| left.path.cmp(&right.path));

            // A replacement may claim a managed link name only when this
            // package's own exact link currently occupies it; anything else is
            // foreign and is never overwritten. Cross-package claims fail in
            // `link_names`.
            for link in &links {
                if !link_names.insert(link.clone()) {
                    bail!(
                        "more than one package in this upgrade provides '{}/{}'",
                        link.subdir,
                        link.name
                    );
                }
                if let Some(directory) = link_dirs.get(link.subdir) {
                    let existing = directory.join(&link.name);
                    match fs::symlink_metadata(&existing) {
                        Ok(_) => {
                            if !owned_links.iter().any(|owned| owned.path == existing) {
                                bail!(
                                    "refusing to upgrade '{}': '{}' already exists and is not a link owned by {} {}; refusing to overwrite it",
                                    old.name,
                                    existing.display(),
                                    old.name,
                                    old.version
                                );
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("failed to inspect '{}'", existing.display())
                            });
                        }
                    }
                }
            }

            upgrades.push(Upgrade {
                old: Removal {
                    receipt_path,
                    package,
                    keg,
                    links: owned_links,
                },
                old_receipt: old,
                retain: false,
                pending: Pending {
                    receipt,
                    stage,
                    links,
                },
            });
        }

        // Decide retention before validation. An upgrade whose old receipt is
        // still pinned exactly (name, version, and digest) by a surviving
        // receipt retains the old keg and identity instead of releasing them;
        // the old keg stays in place and its receipt moves to the retained
        // records. A surviving pin that no active or retained record can
        // satisfy still rejects the upgrade, and a replacement pin must be
        // satisfied by an active receipt only, never by a retained keg.
        let survivors: Vec<Receipt> = self
            .installed()?
            .into_iter()
            .filter(|receipt| !names.contains(&receipt.name))
            .collect();
        let pinned_by = |target: &Receipt, owners: &[Receipt]| {
            owners.iter().any(|receipt| {
                receipt.dependencies.iter().any(|pin| {
                    pin.name == target.name
                        && pin.version == target.version
                        && pin.sha256 == target.sha256
                })
            })
        };
        // A pin owner is any surviving active receipt plus every retained
        // record already on disk: a retained keg's recorded identity keeps
        // the versions it pins, so it forces retention in later batches too.
        // (With garbage collection deferred, every retained record counts,
        // which is a safe superset of the still-reachable ones.)
        let mut owners = survivors.clone();
        owners.extend(retained.iter().cloned());
        let mut new_retained: Vec<Receipt> = Vec::new();
        for upgrade in &mut upgrades {
            if pinned_by(&upgrade.old_receipt, &owners) {
                upgrade.retain = true;
                new_retained.push(upgrade.old_receipt.clone());
            }
        }
        // Transitive closure: a retained old receipt's own pins must keep the
        // old receipts they match in this batch, so a chain of retained kegs
        // stays coherent instead of leaving one of them danging.
        loop {
            let kept: Vec<Receipt> = upgrades
                .iter()
                .filter(|upgrade| upgrade.retain)
                .map(|upgrade| upgrade.old_receipt.clone())
                .collect();
            let mut changed = false;
            for upgrade in &mut upgrades {
                if upgrade.retain {
                    continue;
                }
                if pinned_by(&upgrade.old_receipt, &kept) {
                    upgrade.retain = true;
                    new_retained.push(upgrade.old_receipt.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut all_retained = retained;
        all_retained.extend(new_retained);
        let replacements: Vec<Receipt> = upgrades
            .iter()
            .map(|upgrade| upgrade.pending.receipt.clone())
            .collect();
        validate_dependencies_with_retained(&survivors, &replacements, &all_retained)?;
        let mut final_receipts = survivors.clone();
        final_receipts.extend(replacements.iter().cloned());
        validate_retained_pin_closure(&all_retained, &final_receipts, &all_retained)?;
        verify_retained_pin_kegs(&owners, &final_receipts, &all_retained, &cellar)?;
        validate_conflicts(&final_receipts)?;
        Ok(upgrades)
    }
}
