//! Filesystem, path, and receipt helpers shared by cask transactions.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use super::CaskStoreError;
use sha2::{Digest, Sha256};

use super::{CaskReceipt, EXDEV_ERRNO, MAX_CASK_RECEIPT_BYTES, MAX_VERSION_LEN};
use crate::domain::cask_plan::validate_cask_identifier;
use crate::domain::validate::is_lower_hex_digest;
use crate::infra::store::{ensure_directory, monotonic_nonce, stages_dir, write_new_file_atomic};

pub(super) fn install_app_dir(src: &Path, dst: &Path) -> Result<(), CaskStoreError> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device(&err) => {
            let parent = dst.parent().ok_or(CaskStoreError::AppDestinationNoParent)?;
            let sibling = parent.join(format!(
                ".brewrs-stage-{}-{}",
                std::process::id(),
                monotonic_nonce()
            ));
            let status = Command::new("/usr/bin/ditto")
                .arg(src)
                .arg(&sibling)
                .stdin(Stdio::null())
                .status()
                .map_err(|source| CaskStoreError::DittoSpawn { source })?;
            if !status.success() {
                let _ = fs::remove_dir_all(&sibling);
                return Err(CaskStoreError::DittoFailed { status });
            }
            if let Err(err) = ensure_absent(dst) {
                let _ = fs::remove_dir_all(&sibling);
                return Err(err);
            }
            if let Err(err) = fs::rename(&sibling, dst) {
                let _ = fs::remove_dir_all(&sibling);
                return Err(CaskStoreError::io("failed to publish app bundle at", &dst)(
                    err,
                ));
            }
            let _ = fs::remove_dir_all(src);
            Ok(())
        }
        Err(err) => Err(CaskStoreError::InstallApp {
            from: src.to_path_buf(),
            to: dst.to_path_buf(),
            source: err,
        }),
    }
}

pub(super) fn is_cross_device(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(EXDEV_ERRNO)
    }
    #[cfg(not(unix))]
    {
        let _ = err;
        false
    }
}

pub(super) fn validate_stage_ownership(prefix: &Path, stage: &Path) -> Result<(), CaskStoreError> {
    let stages = stages_dir(prefix)
        .canonicalize()
        .map_err(|source| CaskStoreError::StagesResolve { source })?;
    let actual = stage
        .canonicalize()
        .map_err(CaskStoreError::io("failed to resolve stage", &stage))?;
    if actual.parent() != Some(stages.as_path()) {
        return Err(CaskStoreError::StageOutside {
            stage: stage.to_path_buf(),
        });
    }
    Ok(())
}

pub(super) fn ensure_absent(path: &Path) -> Result<(), CaskStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(CaskStoreError::PathExists {
            path: path.to_path_buf(),
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CaskStoreError::io("failed to inspect", &path)(err)),
    }
}

pub(super) fn ensure_prefix_dest_absent(prefix: &Path, rel: &str) -> Result<(), CaskStoreError> {
    let rel_path = Path::new(rel);
    let mut cur = prefix.to_path_buf();
    for component in rel_path.components() {
        let Component::Normal(part) = component else {
            return Err(CaskStoreError::UnsafeDestination {
                dest: rel.to_string(),
            });
        };
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) => {
                let is_last = cur == prefix.join(rel_path);
                if is_last || meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(CaskStoreError::DestinationConflict {
                        dest: rel.to_string(),
                        path: cur.to_path_buf(),
                    });
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(CaskStoreError::io("failed to inspect", &cur)(err));
            }
        }
    }
    Ok(())
}

pub(super) fn safe_prefix_join_and_mkdirs(
    prefix: &Path,
    rel: &str,
) -> Result<PathBuf, CaskStoreError> {
    let rel_path = Path::new(rel);
    let components: Vec<_> = rel_path.components().collect();
    if components.is_empty() {
        return Err(CaskStoreError::EmptyDestination);
    }
    let mut cur = prefix.to_path_buf();
    for (idx, comp) in components.iter().enumerate() {
        let Component::Normal(part) = comp else {
            return Err(CaskStoreError::NonRelativeDestination {
                dest: rel.to_string(),
            });
        };
        cur.push(part);
        let is_last = idx + 1 == components.len();
        if is_last {
            ensure_absent(&cur)?;
        } else {
            ensure_directory(&cur, true)?;
        }
    }
    Ok(cur)
}

pub(super) fn safe_prefix_join_existing_no_symlinks(
    prefix: &Path,
    rel: &str,
) -> Result<PathBuf, CaskStoreError> {
    let rel_path = Path::new(rel);
    let components: Vec<_> = rel_path.components().collect();
    if components.is_empty() {
        return Err(CaskStoreError::EmptyDestination);
    }
    let mut cur = prefix.to_path_buf();
    for (idx, comp) in components.iter().enumerate() {
        let Component::Normal(part) = comp else {
            return Err(CaskStoreError::UnsafeReceiptDestination {
                dest: rel.to_string(),
            });
        };
        cur.push(part);
        let is_last = idx + 1 == components.len();
        if !is_last {
            let meta = fs::symlink_metadata(&cur).map_err(|source| {
                CaskStoreError::MissingParentDirectory {
                    path: cur.clone(),
                    dest: rel.to_string(),
                    source,
                }
            })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(CaskStoreError::ParentNotRealDirectory {
                    path: cur.to_path_buf(),
                });
            }
        }
    }
    Ok(cur)
}

pub(super) fn verify_regular_file_sha256(
    path: &Path,
    expected: &str,
) -> Result<(), CaskStoreError> {
    let meta = fs::symlink_metadata(path).map_err(|source| CaskStoreError::MissingFile {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(CaskStoreError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    let bytes = fs::read(path).map_err(CaskStoreError::io("failed to read", &path))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        return Err(CaskStoreError::Sha256Mismatch {
            path: path.to_path_buf(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

pub(super) fn write_cask_receipt(
    directory: &Path,
    receipt: &CaskReceipt,
) -> Result<(), CaskStoreError> {
    let target = directory.join(format!("{}.json", receipt.token));
    let mut bytes =
        serde_json::to_vec(receipt).map_err(|source| CaskStoreError::ReceiptEncode {
            token: receipt.token.clone(),
            source,
        })?;
    bytes.push(b'\n');
    write_new_file_atomic(directory, &target, &bytes)?;
    Ok(())
}

pub(super) fn read_cask_receipt(path: &Path) -> Result<CaskReceipt, CaskStoreError> {
    let meta =
        fs::symlink_metadata(path).map_err(CaskStoreError::io("failed to inspect", &path))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(CaskStoreError::ReceiptNotRegular {
            path: path.to_path_buf(),
        });
    }
    if meta.len() > MAX_CASK_RECEIPT_BYTES {
        return Err(CaskStoreError::ReceiptTooLarge {
            path: path.to_path_buf(),
        });
    }
    let bytes = fs::read(path).map_err(CaskStoreError::io("failed to read", &path))?;
    let receipt: CaskReceipt =
        serde_json::from_slice(&bytes).map_err(|source| CaskStoreError::ReceiptDecode {
            path: path.to_path_buf(),
            source,
        })?;
    validate_cask_receipt(&receipt)?;
    Ok(receipt)
}

pub(super) fn validate_cask_receipt(receipt: &CaskReceipt) -> Result<(), CaskStoreError> {
    validate_cask_identifier(&receipt.token, "cask token").map_err(|error| {
        CaskStoreError::InvalidIdentifier {
            message: error.to_string(),
        }
    })?;
    if receipt.version.is_empty()
        || receipt.version.len() > MAX_VERSION_LEN
        || receipt.version == "."
        || receipt.version.contains("..")
        || receipt.version.contains(['/', '\\'])
        || receipt
            .version
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(CaskStoreError::InvalidVersion {
            version: receipt.version.to_string(),
            token: receipt.token.to_string(),
        });
    }
    if !is_lower_hex_digest(&receipt.sha256) {
        return Err(CaskStoreError::InvalidSha256 {
            token: receipt.token.to_string(),
        });
    }
    for app in &receipt.apps {
        if !app.path.is_absolute() || !app.bundle_name.ends_with(".app") {
            return Err(CaskStoreError::InvalidAppRecord {
                token: receipt.token.to_string(),
            });
        }
    }
    for link in &receipt.links {
        validate_relative_receipt_path(&link.dest, &receipt.token)?;
    }
    for wrapper in &receipt.wrappers {
        validate_relative_receipt_path(&wrapper.dest, &receipt.token)?;
        if !is_lower_hex_digest(&wrapper.sha256) {
            return Err(CaskStoreError::InvalidWrapperSha256 {
                token: receipt.token.to_string(),
            });
        }
    }
    for file in &receipt.caskroom_files {
        validate_relative_receipt_path(file, &receipt.token)?;
    }
    Ok(())
}

fn validate_relative_receipt_path(path: &str, token: &str) -> Result<(), CaskStoreError> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', '\0']) {
        return Err(CaskStoreError::InvalidPath {
            path: path.to_string(),
            token: token.to_string(),
        });
    }
    for comp in Path::new(path).components() {
        if !matches!(comp, Component::Normal(_)) {
            return Err(CaskStoreError::UnsafePath {
                path: path.to_string(),
                token: token.to_string(),
            });
        }
    }
    Ok(())
}
