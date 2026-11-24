use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::domain::tap_manifest::TapReceipt;
use crate::infra::tap_artifact::{hash_regular_file, verify_artifact};

use super::fsguard::{directory_entries_exact, verify_ancestor_dirs};

pub(super) fn verify_installed_state(prefix: &Path, receipt: &TapReceipt) -> Result<()> {
    let link_rel = Path::new(&receipt.link);
    let link = prefix.join(link_rel);
    if !verify_ancestor_dirs(prefix, link_rel, "tap link")? {
        bail!(
            "refusing '{}': the tap link parent directory is missing",
            link.display()
        );
    }
    let metadata = fs::symlink_metadata(&link)
        .with_context(|| format!("failed to inspect '{}'", link.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!(
            "refusing '{}': the tap link is not a symlink",
            link.display()
        );
    }
    let expected_target = Path::new("..")
        .join(&receipt.keg)
        .join("bin")
        .join(&receipt.bin);
    let actual_target =
        fs::read_link(&link).with_context(|| format!("failed to read '{}'", link.display()))?;
    if actual_target.as_os_str() != expected_target.as_os_str() {
        bail!(
            "refusing '{}': link target is '{}', not '{}'",
            link.display(),
            actual_target.display(),
            expected_target.display()
        );
    }

    let executable_rel = Path::new(&receipt.keg).join("bin").join(&receipt.bin);
    let executable = prefix.join(&executable_rel);
    if !verify_ancestor_dirs(prefix, &executable_rel, "tap executable")? {
        bail!(
            "refusing '{}': the tap executable parent directory is missing",
            executable.display()
        );
    }
    let metadata = fs::symlink_metadata(&executable)
        .with_context(|| format!("failed to inspect '{}'", executable.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing '{}': the tap executable is not a regular file",
            executable.display()
        );
    }
    if metadata.len() != receipt.artifact_bytes {
        bail!(
            "refusing '{}': size {} does not match the receipt's {} bytes",
            executable.display(),
            metadata.len(),
            receipt.artifact_bytes
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!(
                "refusing '{}': the tap executable is not executable",
                executable.display()
            );
        }
    }
    Ok(())
}

pub(super) fn verify_installed_artifact(prefix: &Path, receipt: &TapReceipt) -> Result<u64> {
    verify_installed_state(prefix, receipt)?;
    let executable = prefix.join(&receipt.keg).join("bin").join(&receipt.bin);
    Ok(verify_artifact(
        &executable,
        &receipt.artifact_sha256,
        &receipt.architecture,
    )?)
}

pub(super) fn verify_removal_state(prefix: &Path, receipt: &TapReceipt) -> Result<()> {
    receipt.validate()?;
    let link_rel = Path::new(&receipt.link);
    let link = prefix.join(link_rel);
    if verify_ancestor_dirs(prefix, link_rel, "tap link")? {
        match fs::symlink_metadata(&link) {
            Ok(metadata) if !metadata.file_type().is_symlink() => bail!(
                "refusing to remove '{}': it is not a symlink",
                link.display()
            ),
            Ok(_) => {
                let expected_target = Path::new("..")
                    .join(&receipt.keg)
                    .join("bin")
                    .join(&receipt.bin);
                let actual_target = fs::read_link(&link)
                    .with_context(|| format!("failed to read '{}'", link.display()))?;
                if actual_target.as_os_str() != expected_target.as_os_str() {
                    bail!(
                        "refusing to remove '{}': link target is '{}', not '{}'",
                        link.display(),
                        actual_target.display(),
                        expected_target.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", link.display()));
            }
        }
    }

    let keg_rel = Path::new(&receipt.keg);
    let keg = prefix.join(keg_rel);
    if verify_ancestor_dirs(prefix, keg_rel, "tap keg")? {
        match fs::symlink_metadata(&keg) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
                "refusing to remove '{}': the tap keg is not a real directory",
                keg.display()
            ),
            Ok(_) => {
                directory_entries_exact(&keg, &["bin"], "tap keg")?;
                let bin_dir = keg.join("bin");
                directory_entries_exact(&bin_dir, &[&receipt.bin], "tap executable directory")?;
                let executable_rel = keg_rel.join("bin").join(&receipt.bin);
                let executable = prefix.join(&executable_rel);
                if !verify_ancestor_dirs(prefix, &executable_rel, "tap executable")? {
                    bail!(
                        "refusing to remove '{}': the tap executable parent directory is missing",
                        executable.display()
                    );
                }
                let metadata = fs::symlink_metadata(&executable)
                    .with_context(|| format!("failed to inspect '{}'", executable.display()))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!(
                        "refusing to remove '{}': it is not a regular file",
                        executable.display()
                    );
                }
                hash_regular_file(
                    &executable,
                    &receipt.artifact_sha256,
                    crate::domain::tap_manifest::MAX_ARTIFACT_BYTES,
                    "installed tap artifact",
                )?;
                let package_dir = keg.parent().context("tap keg has no package directory")?;
                directory_entries_exact(package_dir, &[&receipt.version], "tap package directory")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", keg.display()));
            }
        }
    }
    Ok(())
}
