//! Receipt types, receipt files, and receipt validation.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::Ordering;

use super::StoreError;
use serde::{Deserialize, Serialize};

use super::paths::{monotonic_nonce, sync_directory};
use super::{COUNTER, MAX_NAME_LEN, MAX_RECEIPT_BYTES};
use crate::domain::plan::PackageSource;
use crate::domain::validate::{is_lower_hex_digest, is_safe_version};

/// One direct dependency, pinned to an exact name, version, and digest.
///
/// A pin is satisfied only by a receipt with the same name, the same version,
/// and the same bottle digest; a version bump is a different pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Dependency {
    pub name: String,
    pub version: String,
    pub sha256: String,
}

/// The durable record of one installed package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub version_scheme: u32,
    #[serde(default)]
    pub source: PackageSource,
    pub sha256: String,
    pub requested: bool,
    #[serde(default)]
    pub keg_only: bool,
    #[serde(default)]
    pub conflicts_with: Vec<String>,
    /// Direct dependencies, each pinned to an exact installed name, version,
    /// and digest. Receipts written before dependencies existed omit this
    /// field and decode as empty.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

/// A fully staged package awaiting [`Store::commit`].
pub(crate) struct Prepared {
    pub receipt: Receipt,
    /// Root of the unpacked keg; it must come from [`Store::stage`].
    pub stage: tempfile::TempDir,
}

/// Atomically publish a new file inside `directory` without overwriting `target`.
pub(crate) fn write_new_file_atomic(
    directory: &Path,
    target: &Path,
    bytes: &[u8],
) -> Result<(), StoreError> {
    match fs::symlink_metadata(target) {
        Ok(_) => {
            return Err(StoreError::ReceiptExists {
                path: target.to_path_buf(),
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(StoreError::io("failed to inspect", &target)(error));
        }
    }

    let temporary = directory.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        monotonic_nonce(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> Result<(), StoreError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(StoreError::io("failed to create", &temporary))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(StoreError::io("failed to write", &temporary))?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(StoreError::io("failed to publish", &target)(error));
    }
    sync_directory(directory);
    Ok(())
}

/// Atomically publish a receipt; never replace an existing one.
pub(super) fn write_receipt(directory: &Path, receipt: &Receipt) -> Result<(), StoreError> {
    let target = directory.join(format!("{}.json", receipt.name));
    let bytes = encode_receipt(receipt)?;
    write_new_file_atomic(directory, &target, &bytes)
}

/// Encode a receipt exactly as it is stored on disk.
pub(super) fn encode_receipt(receipt: &Receipt) -> Result<Vec<u8>, StoreError> {
    let mut bytes = serde_json::to_vec(receipt).map_err(|source| StoreError::ReceiptEncode {
        name: receipt.name.clone(),
        source,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Atomically replace one existing regular receipt with exact bytes.
///
/// The replacement is published from a temporary file in the same directory, so
/// a reader sees either the old bytes or the new bytes, never a partial receipt.
pub(super) fn replace_receipt(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let directory = path.parent().ok_or_else(|| StoreError::ReceiptNoParent {
        path: path.to_path_buf(),
    })?;
    let metadata =
        fs::symlink_metadata(path).map_err(StoreError::io("failed to inspect", &path))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::ReplaceNotRegular {
            path: path.to_path_buf(),
        });
    }
    let temporary = directory.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        monotonic_nonce(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> Result<(), StoreError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(StoreError::io("failed to create", &temporary))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(StoreError::io("failed to write", &temporary))?;
        fs::set_permissions(&temporary, metadata.permissions()).map_err(StoreError::io(
            "failed to preserve the permissions of",
            &path,
        ))?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(StoreError::io("failed to publish", &path)(error));
    }
    sync_directory(directory);
    Ok(())
}

/// Read and validate one receipt file without following symlinks.
pub(super) fn read_receipt(path: &Path) -> Result<Receipt, StoreError> {
    read_receipt_with_bytes(path).map(|(receipt, _)| receipt)
}

/// Read one receipt together with its exact bytes, which a promotion restores on
/// rollback.
pub(super) fn read_receipt_with_bytes(path: &Path) -> Result<(Receipt, Vec<u8>), StoreError> {
    let metadata =
        fs::symlink_metadata(path).map_err(StoreError::io("failed to inspect", &path))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::ReceiptNotRegular {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > MAX_RECEIPT_BYTES {
        return Err(StoreError::ReceiptTooLarge {
            path: path.to_path_buf(),
        });
    }
    let bytes = fs::read(path).map_err(StoreError::io("failed to read", &path))?;
    let receipt: Receipt =
        serde_json::from_slice(&bytes).map_err(|source| StoreError::ReceiptDecode {
            path: path.to_path_buf(),
            source,
        })?;
    validate_receipt(&receipt)?;
    Ok((receipt, bytes))
}

pub(super) fn existing_receipt(path: &Path) -> Result<Option<Receipt>, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_receipt(path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StoreError::io("failed to inspect", &path)(error)),
    }
}

/// Validate a package name exactly as the archive layer does.
pub(super) fn is_safe_package_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN || name.contains("..") {
        return false;
    }
    let bytes = name.as_bytes();
    (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'@' | b'+' | b'-' | b'_' | b'.')
        })
}

/// Bin members become single path components, so reject anything surprising.
pub(super) fn is_safe_bin_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 || name.contains("..") {
        return false;
    }
    let bytes = name.as_bytes();
    (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'+' | b'-' | b'_' | b'.')
        })
}

pub(super) fn validate_receipt(receipt: &Receipt) -> Result<(), StoreError> {
    if !is_safe_package_name(&receipt.name) {
        return Err(StoreError::InvalidPackageName {
            name: receipt.name.to_string(),
        });
    }
    if !is_safe_version(&receipt.version) {
        return Err(StoreError::InvalidVersion {
            name: receipt.name.to_string(),
            version: receipt.version.to_string(),
        });
    }
    if !is_lower_hex_digest(&receipt.sha256) {
        return Err(StoreError::InvalidSha256 {
            name: receipt.name.to_string(),
        });
    }
    let mut names = HashSet::new();
    for dependency in &receipt.dependencies {
        validate_dependency(&receipt.name, dependency)?;
        if dependency.name == receipt.name {
            return Err(StoreError::SelfDependency {
                name: receipt.name.to_string(),
            });
        }
        if !names.insert(dependency.name.as_str()) {
            return Err(StoreError::DuplicateDependency {
                name: receipt.name.to_string(),
                dependency: dependency.name.to_string(),
            });
        }
    }
    let mut conflicts = HashSet::new();
    for conflict in &receipt.conflicts_with {
        if !is_safe_package_name(conflict) {
            return Err(StoreError::InvalidConflictName {
                owner: receipt.name.to_string(),
                conflict: conflict.to_string(),
            });
        }
        if conflict == &receipt.name {
            return Err(StoreError::SelfConflict {
                name: receipt.name.to_string(),
            });
        }
        if !conflicts.insert(conflict.as_str()) {
            return Err(StoreError::DuplicateConflict {
                name: receipt.name.to_string(),
                conflict: conflict.to_string(),
            });
        }
    }
    Ok(())
}

/// Validate one dependency pin recorded by `owner`.
fn validate_dependency(owner: &str, dependency: &Dependency) -> Result<(), StoreError> {
    if !is_safe_package_name(&dependency.name) {
        return Err(StoreError::InvalidDependencyName {
            owner: owner.to_string(),
            name: dependency.name.to_string(),
        });
    }
    if !is_safe_version(&dependency.version) {
        return Err(StoreError::InvalidDependencyVersion {
            owner: owner.to_string(),
            name: dependency.name.to_string(),
            version: dependency.version.to_string(),
        });
    }
    if !is_lower_hex_digest(&dependency.sha256) {
        return Err(StoreError::InvalidDependencySha256 {
            owner: owner.to_string(),
            name: dependency.name.to_string(),
        });
    }
    Ok(())
}
