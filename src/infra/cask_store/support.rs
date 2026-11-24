//! Filesystem, path, and receipt helpers shared by cask transactions.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::{CaskReceipt, EXDEV_ERRNO, MAX_CASK_RECEIPT_BYTES, MAX_VERSION_LEN};
use crate::domain::cask_plan::validate_cask_identifier;
use crate::domain::validate::is_lower_hex_digest;
use crate::infra::store::{ensure_directory, monotonic_nonce, stages_dir, write_new_file_atomic};

pub(super) fn install_app_dir(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device(&err) => {
            let parent = dst
                .parent()
                .context("app destination has no parent directory")?;
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
                .context("failed to run /usr/bin/ditto into custom --appdir")?;
            if !status.success() {
                let _ = fs::remove_dir_all(&sibling);
                bail!("/usr/bin/ditto failed copying app bundle to --appdir ({status})");
            }
            if let Err(err) = ensure_absent(dst) {
                let _ = fs::remove_dir_all(&sibling);
                return Err(err);
            }
            if let Err(err) = fs::rename(&sibling, dst) {
                let _ = fs::remove_dir_all(&sibling);
                return Err(err).with_context(|| {
                    format!("failed to publish app bundle at '{}'", dst.display())
                });
            }
            let _ = fs::remove_dir_all(src);
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to install app bundle '{}' into '{}'",
                src.display(),
                dst.display()
            )
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

pub(super) fn validate_stage_ownership(prefix: &Path, stage: &Path) -> Result<()> {
    let stages = stages_dir(prefix)
        .canonicalize()
        .context("failed to resolve brewrs stages directory")?;
    let actual = stage
        .canonicalize()
        .with_context(|| format!("failed to resolve stage '{}'", stage.display()))?;
    if actual.parent() != Some(stages.as_path()) {
        bail!(
            "refusing cask stage '{}' outside brewrs stages",
            stage.display()
        );
    }
    Ok(())
}

pub(super) fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("refusing to overwrite existing path '{}'", path.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

pub(super) fn ensure_prefix_dest_absent(prefix: &Path, rel: &str) -> Result<()> {
    let rel_path = Path::new(rel);
    let mut cur = prefix.to_path_buf();
    for component in rel_path.components() {
        let Component::Normal(part) = component else {
            bail!("refusing unsafe cask destination '{rel}'");
        };
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) => {
                let is_last = cur == prefix.join(rel_path);
                if is_last || meta.file_type().is_symlink() || !meta.is_dir() {
                    bail!(
                        "refusing cask destination '{}': conflicts with existing path '{}'",
                        rel,
                        cur.display()
                    );
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to inspect '{}'", cur.display()));
            }
        }
    }
    Ok(())
}

pub(super) fn safe_prefix_join_and_mkdirs(prefix: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    let components: Vec<_> = rel_path.components().collect();
    if components.is_empty() {
        bail!("empty prefix relative destination");
    }
    let mut cur = prefix.to_path_buf();
    for (idx, comp) in components.iter().enumerate() {
        let Component::Normal(part) = comp else {
            bail!("refusing non-relative destination '{rel}'");
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

pub(super) fn safe_prefix_join_existing_no_symlinks(prefix: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    let components: Vec<_> = rel_path.components().collect();
    if components.is_empty() {
        bail!("empty prefix relative destination");
    }
    let mut cur = prefix.to_path_buf();
    for (idx, comp) in components.iter().enumerate() {
        let Component::Normal(part) = comp else {
            bail!("refusing unsafe destination '{rel}' in receipt");
        };
        cur.push(part);
        let is_last = idx + 1 == components.len();
        if !is_last {
            let meta = fs::symlink_metadata(&cur)
                .with_context(|| format!("missing directory '{}' for '{rel}'", cur.display()))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!(
                    "refusing '{}': parent directory is not a real directory",
                    cur.display()
                );
            }
        }
    }
    Ok(cur)
}

pub(super) fn verify_regular_file_sha256(path: &Path, expected: &str) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("missing file '{}'", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!("'{}' is not a regular file", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        bail!(
            "SHA-256 mismatch at '{}': expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

pub(super) fn write_cask_receipt(directory: &Path, receipt: &CaskReceipt) -> Result<()> {
    let target = directory.join(format!("{}.json", receipt.token));
    let mut bytes = serde_json::to_vec(receipt)
        .with_context(|| format!("failed to encode cask receipt '{}'", receipt.token))?;
    bytes.push(b'\n');
    write_new_file_atomic(directory, &target, &bytes)
}

pub(super) fn read_cask_receipt(path: &Path) -> Result<CaskReceipt> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect '{}'", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!(
            "refusing '{}': cask receipt must be a regular file",
            path.display()
        );
    }
    if meta.len() > MAX_CASK_RECEIPT_BYTES {
        bail!("refusing '{}': cask receipt is too large", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    let receipt: CaskReceipt = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode cask receipt '{}'", path.display()))?;
    validate_cask_receipt(&receipt)?;
    Ok(receipt)
}

pub(super) fn validate_cask_receipt(receipt: &CaskReceipt) -> Result<()> {
    validate_cask_identifier(&receipt.token, "cask token")?;
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
        bail!(
            "invalid version '{}' in cask receipt '{}'",
            receipt.version,
            receipt.token
        );
    }
    if !is_lower_hex_digest(&receipt.sha256) {
        bail!("invalid sha256 in cask receipt '{}'", receipt.token);
    }
    for app in &receipt.apps {
        if !app.path.is_absolute() || !app.bundle_name.ends_with(".app") {
            bail!("invalid app record in cask receipt '{}'", receipt.token);
        }
    }
    for link in &receipt.links {
        validate_relative_receipt_path(&link.dest, &receipt.token)?;
    }
    for wrapper in &receipt.wrappers {
        validate_relative_receipt_path(&wrapper.dest, &receipt.token)?;
        if !is_lower_hex_digest(&wrapper.sha256) {
            bail!("invalid wrapper sha256 in cask receipt '{}'", receipt.token);
        }
    }
    for file in &receipt.caskroom_files {
        validate_relative_receipt_path(file, &receipt.token)?;
    }
    Ok(())
}

fn validate_relative_receipt_path(path: &str, token: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\\', '\0']) {
        bail!("invalid path '{path}' in cask receipt '{token}'");
    }
    for comp in Path::new(path).components() {
        if !matches!(comp, Component::Normal(_)) {
            bail!("unsafe path '{path}' in cask receipt '{token}'");
        }
    }
    Ok(())
}
