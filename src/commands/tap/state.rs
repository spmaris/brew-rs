use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::domain::tap_manifest::{
    MAX_MANIFEST_BYTES, MAX_PACKAGES, MAX_RECEIPT_BYTES, MAX_RECORD_BYTES, MAX_TAPS, TapManifest,
    TapReceipt, TapRecord, parse_tap_name,
};
use crate::infra::store::state_dir;

use super::fsguard::{ensure_real_dir, read_bounded_regular, read_dir_names, verify_ancestor_dirs};

/// Prefix-relative directory that holds registered tap state.
pub(super) const TAPS: &str = "taps";

/// Registry record file name.
pub(super) const RECORD_FILE: &str = "tap.json";

/// Exact manifest copy file name.
pub(super) const MANIFEST_FILE: &str = "manifest.toml";

/// Per-tap receipt directory name.
pub(super) const RECEIPTS: &str = "receipts";

/// A validated tap and its exact manifest copy.
pub(super) struct TappedTap {
    pub(super) record: TapRecord,
    pub(super) manifest: TapManifest,
    pub(super) dir: PathBuf,
    pub(super) receipts: PathBuf,
}

impl TappedTap {
    pub(super) fn manifest_sha256(&self) -> &str {
        &self.record.manifest_sha256
    }
}

pub(super) fn receipt_relative_path(receipt: &TapReceipt) -> Result<PathBuf> {
    let (owner, repo) = parse_tap_name(&receipt.tap)?;
    Ok(Path::new(".brewrs")
        .join("state")
        .join(TAPS)
        .join(owner)
        .join(repo)
        .join(RECEIPTS)
        .join(format!("{}.json", receipt.token)))
}

pub(super) fn read_tapped(prefix: &Path, tap: &str) -> Result<TappedTap> {
    let (owner, repo) = parse_tap_name(tap)?;
    let dir = tap_dir(prefix, &owner, &repo);
    let record_rel = Path::new(".brewrs")
        .join("state")
        .join(TAPS)
        .join(&owner)
        .join(&repo)
        .join(RECORD_FILE);
    if !verify_ancestor_dirs(prefix, &record_rel, "tap state")? {
        bail!(
            "tap '{tap}' is not registered in '{}'; run `brewrs tap add {tap} --manifest <path>`",
            prefix.display()
        );
    }
    ensure_real_dir(&dir, "tap directory").with_context(|| {
        format!(
            "tap '{tap}' is not registered in '{}'; run `brewrs tap add {tap} --manifest <path>`",
            prefix.display()
        )
    })?;

    let record_path = dir.join(RECORD_FILE);
    let record_bytes = read_bounded_regular(&record_path, MAX_RECORD_BYTES, "tap registry record")?;
    let record: TapRecord = serde_json::from_slice(&record_bytes)
        .with_context(|| format!("failed to decode '{}'", record_path.display()))?;
    record.validate(tap)?;

    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest_bytes =
        read_bounded_regular(&manifest_path, MAX_MANIFEST_BYTES, "tap manifest copy")?;
    let digest = sha256_hex(&manifest_bytes);
    if digest != record.manifest_sha256 {
        bail!(
            "tap '{tap}' manifest copy changed since it was registered (expected sha256 {}, got {digest}); run `brewrs untap {tap}` and add it again",
            record.manifest_sha256
        );
    }
    let manifest = TapManifest::parse(&manifest_bytes, tap)
        .with_context(|| format!("failed to validate '{}'", manifest_path.display()))?;

    let receipts = dir.join(RECEIPTS);
    match fs::symlink_metadata(&receipts) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the tap receipt root is not a real directory",
            receipts.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", receipts.display()));
        }
    }

    Ok(TappedTap {
        receipts,
        dir,
        record,
        manifest,
    })
}

pub(super) fn all_tapped(prefix: &Path) -> Result<Vec<TappedTap>> {
    let root = taps_root(prefix);
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the tap state root is not a real directory",
            root.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", root.display()));
        }
    }

    let owners = read_dir_names(&root, MAX_TAPS, "tap owner")?;
    let mut taps = Vec::new();
    for owner in owners {
        let owner_dir = root.join(&owner);
        ensure_real_dir(&owner_dir, "tap owner directory")?;
        let repos = read_dir_names(&owner_dir, MAX_TAPS, "tap repository")?;
        for repo in repos {
            let dir = owner_dir.join(&repo);
            ensure_real_dir(&dir, "tap directory")?;
            let entries = read_dir_names(&dir, MAX_PACKAGES + 3, "tap state entry")?;
            if entries.is_empty() {
                continue;
            }
            let tap = format!("{owner}/{repo}");
            taps.push(read_tapped(prefix, &tap)?);
            if taps.len() > MAX_TAPS {
                bail!(
                    "more than {MAX_TAPS} taps are registered in '{}'",
                    prefix.display()
                );
            }
        }
    }
    Ok(taps)
}

pub(super) fn read_receipts(tapped: &TappedTap) -> Result<Vec<TapReceipt>> {
    match fs::symlink_metadata(&tapped.receipts) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing '{}': the tap receipt root is not a real directory",
            tapped.receipts.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", tapped.receipts.display()));
        }
    }

    let entries = read_dir_names(&tapped.receipts, MAX_PACKAGES + 1, "tap receipt")?;
    let mut receipts = Vec::new();
    for entry in entries {
        let Some(stem) = entry.strip_suffix(".json") else {
            bail!(
                "refusing '{}': unexpected tap receipt entry '{entry}'",
                tapped.receipts.display()
            );
        };
        let path = tapped.receipts.join(&entry);
        let Some(receipt) = read_optional_receipt(&path)? else {
            continue;
        };
        if receipt.tap != tapped.record.tap || receipt.token != stem {
            bail!(
                "tap receipt '{}' does not match tap '{}' and token '{stem}'",
                path.display(),
                tapped.record.tap
            );
        }
        receipts.push(receipt);
    }
    Ok(receipts)
}

pub(super) fn read_optional_receipt(path: &Path) -> Result<Option<TapReceipt>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", path.display()));
        }
    }
    let bytes = read_bounded_regular(path, MAX_RECEIPT_BYTES, "tap receipt")?;
    let receipt: TapReceipt = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode the tap receipt '{}'", path.display()))?;
    receipt.validate()?;
    Ok(Some(receipt))
}

pub(super) fn read_source_manifest(path: &Path) -> Result<(PathBuf, Vec<u8>)> {
    if path.as_os_str().is_empty() {
        bail!("refusing an empty tap manifest path");
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect tap manifest '{}'", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing tap manifest '{}': the source path is a symlink; pass the real file",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing tap manifest '{}': the source path is not a regular file",
            path.display()
        );
    }
    if metadata.len() == 0 || metadata.len() > MAX_MANIFEST_BYTES {
        bail!(
            "refusing tap manifest '{}': size {} is outside the supported range",
            path.display(),
            metadata.len()
        );
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve tap manifest '{}'", path.display()))?;
    let bytes = read_bounded_regular(&canonical, MAX_MANIFEST_BYTES, "tap manifest")?;
    if bytes.is_empty() {
        bail!("refusing empty tap manifest '{}'", canonical.display());
    }
    Ok((canonical, bytes))
}

pub(super) fn taps_root(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(TAPS)
}

pub(super) fn tap_dir(prefix: &Path, owner: &str, repo: &str) -> PathBuf {
    taps_root(prefix).join(owner).join(repo)
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(super) fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).context("failed to encode tap state")?;
    bytes.push(b'\n');
    Ok(bytes)
}
