//! Receipt types, receipt files, and receipt validation.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
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
pub(crate) fn write_new_file_atomic(directory: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(_) => bail!(
            "refusing to overwrite the existing receipt '{}'",
            target.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", target.display()));
        }
    }

    let temporary = directory.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        monotonic_nonce(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("failed to create '{}'", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("failed to write '{}'", temporary.display()))?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to publish '{}'", target.display()));
    }
    sync_directory(directory);
    Ok(())
}

/// Atomically publish a receipt; never replace an existing one.
pub(super) fn write_receipt(directory: &Path, receipt: &Receipt) -> Result<()> {
    let target = directory.join(format!("{}.json", receipt.name));
    let bytes = encode_receipt(receipt)?;
    write_new_file_atomic(directory, &target, &bytes)
}

/// Encode a receipt exactly as it is stored on disk.
pub(super) fn encode_receipt(receipt: &Receipt) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(receipt)
        .with_context(|| format!("failed to encode the receipt for '{}'", receipt.name))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Atomically replace one existing regular receipt with exact bytes.
///
/// The replacement is published from a temporary file in the same directory, so
/// a reader sees either the old bytes or the new bytes, never a partial receipt.
pub(super) fn replace_receipt(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().with_context(|| {
        format!(
            "refusing to replace '{}': it has no parent directory",
            path.display()
        )
    })?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect '{}'", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing to replace '{}': a receipt must be a regular file",
            path.display()
        );
    }
    let temporary = directory.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        monotonic_nonce(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let written = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("failed to create '{}'", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("failed to write '{}'", temporary.display()))?;
        fs::set_permissions(&temporary, metadata.permissions()).with_context(|| {
            format!("failed to preserve the permissions of '{}'", path.display())
        })?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to publish '{}'", path.display()));
    }
    sync_directory(directory);
    Ok(())
}

/// Read and validate one receipt file without following symlinks.
pub(super) fn read_receipt(path: &Path) -> Result<Receipt> {
    read_receipt_with_bytes(path).map(|(receipt, _)| receipt)
}

/// Read one receipt together with its exact bytes, which a promotion restores on
/// rollback.
pub(super) fn read_receipt_with_bytes(path: &Path) -> Result<(Receipt, Vec<u8>)> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect '{}'", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing '{}': a receipt must be a regular file",
            path.display()
        );
    }
    if metadata.len() > MAX_RECEIPT_BYTES {
        bail!("refusing '{}': the receipt is too large", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let receipt: Receipt = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode the receipt '{}'", path.display()))?;
    validate_receipt(&receipt)?;
    Ok((receipt, bytes))
}

pub(super) fn existing_receipt(path: &Path) -> Result<Option<Receipt>> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_receipt(path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
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

pub(super) fn validate_receipt(receipt: &Receipt) -> Result<()> {
    if !is_safe_package_name(&receipt.name) {
        bail!("refusing invalid package name '{}'", receipt.name);
    }
    if !is_safe_version(&receipt.version) {
        bail!(
            "refusing invalid version '{}' for '{}'",
            receipt.version,
            receipt.name
        );
    }
    if !is_lower_hex_digest(&receipt.sha256) {
        bail!(
            "refusing invalid sha256 for '{}': expected 64 lowercase hexadecimal characters",
            receipt.name
        );
    }
    let mut names = HashSet::new();
    for dependency in &receipt.dependencies {
        validate_dependency(&receipt.name, dependency)?;
        if dependency.name == receipt.name {
            bail!(
                "refusing '{}': it lists itself as a dependency",
                receipt.name
            );
        }
        if !names.insert(dependency.name.as_str()) {
            bail!(
                "refusing '{}': dependency '{}' is listed more than once",
                receipt.name,
                dependency.name
            );
        }
    }
    let mut conflicts = HashSet::new();
    for conflict in &receipt.conflicts_with {
        if !is_safe_package_name(conflict) {
            bail!(
                "refusing invalid conflict name '{}' recorded by '{}'",
                conflict,
                receipt.name
            );
        }
        if conflict == &receipt.name {
            bail!("refusing '{}': it lists itself as a conflict", receipt.name);
        }
        if !conflicts.insert(conflict.as_str()) {
            bail!(
                "refusing '{}': conflict '{}' is listed more than once",
                receipt.name,
                conflict
            );
        }
    }
    Ok(())
}

/// Validate one dependency pin recorded by `owner`.
fn validate_dependency(owner: &str, dependency: &Dependency) -> Result<()> {
    if !is_safe_package_name(&dependency.name) {
        bail!(
            "refusing invalid dependency name '{}' recorded by '{}'",
            dependency.name,
            owner
        );
    }
    if !is_safe_version(&dependency.version) {
        bail!(
            "refusing invalid dependency version '{}' for '{}' recorded by '{}'",
            dependency.version,
            dependency.name,
            owner
        );
    }
    if !is_lower_hex_digest(&dependency.sha256) {
        bail!(
            "refusing invalid sha256 for dependency '{}' recorded by '{}': expected 64 lowercase hexadecimal characters",
            dependency.name,
            owner
        );
    }
    Ok(())
}
