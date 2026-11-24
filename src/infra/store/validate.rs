//! Pure validation of projected receipt graphs.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use super::StoreError;

use super::receipt::{Dependency, Receipt, validate_receipt};

/// Validate a projected receipt graph as a whole.
///
/// This is pure: the caller supplies exactly the receipts the operation would
/// leave installed (commit adds, upgrade replaces, removal subtracts), and no
/// filesystem state is read. Every receipt's own fields are validated, package
/// names must be unique, and each recorded direct dependency must resolve to a
/// receipt with the same name, version, and digest. Errors name both the owner
/// and the affected dependency.
///
/// Production code uses [`validate_dependencies_with_retained`]; this strict
/// active-only form remains for tests of the pure graph rules.
#[cfg(test)]
pub(crate) fn validate_dependencies(receipts: &[Receipt]) -> Result<(), StoreError> {
    let mut installed: HashMap<&str, &Receipt> = HashMap::with_capacity(receipts.len());
    for receipt in receipts {
        validate_receipt(receipt)?;
        if installed.insert(receipt.name.as_str(), receipt).is_some() {
            return Err(StoreError::DuplicateInGraph {
                name: receipt.name.to_string(),
            });
        }
    }
    for receipt in receipts {
        for dependency in &receipt.dependencies {
            let Some(actual) = installed.get(dependency.name.as_str()) else {
                return Err(StoreError::DependencyMissing {
                    owner: receipt.name.to_string(),
                    pin: Box::new(Dependency {
                        name: dependency.name.to_string(),
                        version: dependency.version.to_string(),
                        sha256: dependency.sha256.to_string(),
                    }),
                });
            };
            if actual.version != dependency.version || actual.sha256 != dependency.sha256 {
                return Err(StoreError::DependencyMismatch {
                    owner: receipt.name.to_string(),
                    pin: Box::new(Dependency {
                        name: dependency.name.to_string(),
                        version: dependency.version.to_string(),
                        sha256: dependency.sha256.to_string(),
                    }),
                    actual_version: actual.version.to_string(),
                    actual_sha256: actual.sha256.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Verify that every pin recorded by a retained receipt still resolves to an
/// active receipt or another retained record at its exact version and digest.
///
/// A retained keg is inert but its recorded identity describes what it was
/// built against; a removal or upgrade that would delete the active keg a
/// retained record pins is refused rather than silently stranding the
/// retained keg's recorded dependencies. This is pure: the caller supplies
/// the retained records, the active receipts the operation would leave
/// installed, and every retained record the operation would provide.
pub(crate) fn validate_retained_pin_closure(
    retained: &[Receipt],
    active: &[Receipt],
    retained_set: &[Receipt],
) -> Result<(), StoreError> {
    let active_by_name: HashMap<&str, &Receipt> = HashMap::from_iter(
        active
            .iter()
            .map(|receipt| (receipt.name.as_str(), receipt)),
    );
    let retained_by_name: HashMap<&str, HashSet<(&str, &str)>> =
        retained_set
            .iter()
            .fold(HashMap::new(), |mut by_name, record| {
                by_name
                    .entry(record.name.as_str())
                    .or_default()
                    .insert((record.version.as_str(), record.sha256.as_str()));
                by_name
            });
    for record in retained {
        for pin in &record.dependencies {
            let active_match = active_by_name
                .get(pin.name.as_str())
                .is_some_and(|actual| actual.version == pin.version && actual.sha256 == pin.sha256);
            if active_match {
                continue;
            }
            let retained_match = retained_by_name
                .get(pin.name.as_str())
                .is_some_and(|versions| {
                    versions.contains(&(pin.version.as_str(), pin.sha256.as_str()))
                });
            if !retained_match {
                return Err(StoreError::RetainedPinStranded {
                    record_name: record.name.to_string(),
                    record_version: record.version.to_string(),
                    pin: Box::new(Dependency {
                        name: pin.name.to_string(),
                        version: pin.version.to_string(),
                        sha256: pin.sha256.to_string(),
                    }),
                });
            }
        }
    }
    Ok(())
}

/// Validate a projected receipt graph where pins recorded by *new* receipts
/// may only resolve to active receipts, while pins recorded by surviving
/// receipts may also resolve to retained records.
///
/// This is pure: the caller supplies the receipts the operation would leave
/// installed, split into the receipts that already existed (`receipts`) and
/// the receipts this operation writes (`new_receipts`), plus the retained
/// records on disk (or the records this operation would create). Every
/// receipt's own fields are validated, names must be unique across both
/// slices, and a retained record must be a valid receipt whose exact `(name,
/// version, sha256)` a pin can match.
///
/// A new receipt can never be satisfied by a retained keg: a retained record
/// is the inert identity of a superseded version, not an install target, so
/// an install or upgrade replacement that would pin one is refused explicitly
/// rather than silently binding to it.
pub(crate) fn validate_dependencies_with_retained(
    receipts: &[Receipt],
    new_receipts: &[Receipt],
    retained: &[Receipt],
) -> Result<(), StoreError> {
    let mut active: HashMap<&str, &Receipt> =
        HashMap::with_capacity(receipts.len() + new_receipts.len());
    for receipt in receipts.iter().chain(new_receipts) {
        validate_receipt(receipt)?;
        if active.insert(receipt.name.as_str(), receipt).is_some() {
            return Err(StoreError::DuplicateInGraph {
                name: receipt.name.to_string(),
            });
        }
    }
    let newcomers: HashSet<&str> = new_receipts
        .iter()
        .map(|receipt| receipt.name.as_str())
        .collect();
    let mut retained_by_name: HashMap<&str, HashSet<(&str, &str)>> = HashMap::new();
    for record in retained {
        validate_receipt(record)?;
        if !retained_by_name
            .entry(record.name.as_str())
            .or_default()
            .insert((record.version.as_str(), record.sha256.as_str()))
        {
            return Err(StoreError::DuplicateRetainedRecord {
                name: record.name.to_string(),
                version: record.version.to_string(),
                sha256: record.sha256.to_string(),
            });
        }
    }

    for receipt in receipts.iter().chain(new_receipts) {
        for dependency in &receipt.dependencies {
            let active_match = active.get(dependency.name.as_str()).filter(|actual| {
                actual.version == dependency.version && actual.sha256 == dependency.sha256
            });
            if active_match.is_some() {
                continue;
            }
            let retained_match =
                retained_by_name
                    .get(dependency.name.as_str())
                    .is_some_and(|versions| {
                        versions
                            .contains(&(dependency.version.as_str(), dependency.sha256.as_str()))
                    });
            if newcomers.contains(receipt.name.as_str()) {
                if retained_match {
                    return Err(StoreError::DependencyOnlyRetained {
                        owner: receipt.name.to_string(),
                        pin: Box::new(Dependency {
                            name: dependency.name.to_string(),
                            version: dependency.version.to_string(),
                            sha256: dependency.sha256.to_string(),
                        }),
                    });
                }
                return Err(StoreError::DependencyMissing {
                    owner: receipt.name.to_string(),
                    pin: Box::new(Dependency {
                        name: dependency.name.to_string(),
                        version: dependency.version.to_string(),
                        sha256: dependency.sha256.to_string(),
                    }),
                });
            }
            if !retained_match {
                return Err(StoreError::DependencyNotProvided {
                    owner: receipt.name.to_string(),
                    pin: Box::new(Dependency {
                        name: dependency.name.to_string(),
                        version: dependency.version.to_string(),
                        sha256: dependency.sha256.to_string(),
                    }),
                });
            }
        }
    }
    Ok(())
}

/// Verify that every pin only a retained record can satisfy still owns a real
/// keg directory.
///
/// A pin that matches no active receipt is checked against the retained
/// records, and each such pin must have a real (non-symlinked) keg at
/// `Cellar/<name>/<version>`; a missing or foreign keg means the record alone
/// cannot keep the dependent working, and the transaction fails closed
/// instead of trusting the record.
pub(super) fn verify_retained_pin_kegs(
    receipts: &[Receipt],
    active: &[Receipt],
    retained: &[Receipt],
    cellar: &Path,
) -> Result<(), StoreError> {
    let active_by_name: HashMap<&str, &Receipt> = HashMap::from_iter(
        active
            .iter()
            .map(|receipt| (receipt.name.as_str(), receipt)),
    );
    let retained_by_name: HashMap<&str, HashSet<(&str, &str)>> =
        retained.iter().fold(HashMap::new(), |mut by_name, record| {
            by_name
                .entry(record.name.as_str())
                .or_default()
                .insert((record.version.as_str(), record.sha256.as_str()));
            by_name
        });
    for receipt in receipts {
        for pin in &receipt.dependencies {
            let active_match = active_by_name
                .get(pin.name.as_str())
                .is_some_and(|actual| actual.version == pin.version && actual.sha256 == pin.sha256);
            if active_match {
                continue;
            }
            let retained_match = retained_by_name
                .get(pin.name.as_str())
                .is_some_and(|versions| {
                    versions.contains(&(pin.version.as_str(), pin.sha256.as_str()))
                });
            if !retained_match {
                return Err(StoreError::DependencyUnresolved {
                    owner: receipt.name.to_string(),
                    pin: Box::new(Dependency {
                        name: pin.name.to_string(),
                        version: pin.version.to_string(),
                        sha256: pin.sha256.to_string(),
                    }),
                });
            }
            let keg = cellar.join(&pin.name).join(&pin.version);
            match fs::symlink_metadata(&keg) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(StoreError::RetainedPinKegNotDirectory {
                        owner: receipt.name.to_string(),
                        name: pin.name.to_string(),
                        version: pin.version.to_string(),
                        keg: keg.to_path_buf(),
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(StoreError::RetainedPinKegMissing {
                        owner: receipt.name.to_string(),
                        name: pin.name.to_string(),
                        version: pin.version.to_string(),
                        keg: keg.to_path_buf(),
                    });
                }
                Err(error) => {
                    return Err(StoreError::io("failed to inspect", &keg)(error));
                }
            }
        }
    }
    Ok(())
}

/// Reject any projected receipt set where one package conflicts with another
/// present package. Checks both directions symmetrically so neither installing
/// a conflicting formula nor keeping an older one with a declared conflict can
/// succeed.
pub(crate) fn validate_conflicts(receipts: &[Receipt]) -> Result<(), StoreError> {
    let installed: HashSet<&str> = receipts
        .iter()
        .map(|receipt| receipt.name.as_str())
        .collect();
    for receipt in receipts {
        for conflict in &receipt.conflicts_with {
            if installed.contains(conflict.as_str()) {
                return Err(StoreError::PackageConflict {
                    name: receipt.name.to_string(),
                    conflict: conflict.to_string(),
                });
            }
        }
    }
    Ok(())
}
