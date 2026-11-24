use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::domain::tap_manifest::{TAPROOM, TapReceipt};
use crate::infra::store::{
    Store, create_symlink, ensure_directory, stages_dir, write_new_file_atomic,
};

use super::fsguard::{describe_path, verify_ancestor_dirs};
use super::selection::TapPlan;
use super::state::receipt_relative_path;
use super::verify::verify_removal_state;

pub(super) fn commit_install(
    store: &Store,
    plan: &TapPlan,
    staged: &Path,
    receipt_bytes: &[u8],
) -> Result<()> {
    let prefix = store.prefix();
    let link = prefix.join(&plan.link);
    match fs::symlink_metadata(&link) {
        Ok(metadata) => bail!(
            "refusing to publish '{}': it already exists ({})",
            link.display(),
            describe_path(&metadata)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", link.display()));
        }
    }

    let keg = prefix.join(&plan.keg);
    if fs::symlink_metadata(&keg).is_ok() {
        bail!(
            "refusing to publish '{}': the keg path already exists",
            keg.display()
        );
    }

    let mut created = Vec::new();
    if let Err(error) = ensure_taproom(prefix, plan, &mut created) {
        remove_created_dirs(&created);
        return Err(error);
    }

    let mut link_created = false;
    let mut file_created = false;
    let result = (|| -> Result<()> {
        ensure_directory(&keg, true)?;
        let bin_dir = keg.join("bin");
        ensure_directory(&bin_dir, true)?;
        let executable = bin_dir.join(&plan.bin);
        fs::rename(staged, &executable).with_context(|| {
            format!(
                "failed to publish the tap executable to '{}'",
                executable.display()
            )
        })?;
        file_created = true;

        let target = Path::new("..").join(&plan.keg).join("bin").join(&plan.bin);
        create_symlink(&target, &link)
            .with_context(|| format!("failed to create the tap link '{}'", link.display()))?;
        link_created = true;

        let receipt_dir = plan
            .receipt_path
            .parent()
            .context("tap receipt path has no parent directory")?;
        ensure_directory(receipt_dir, true)?;
        write_new_file_atomic(receipt_dir, &plan.receipt_path, receipt_bytes)?;
        Ok(())
    })();

    if let Err(error) = result {
        let mut issues = Vec::new();
        if link_created && let Err(remove_error) = fs::remove_file(&link) {
            issues.push(format!(
                "failed to remove '{}': {remove_error}",
                link.display()
            ));
        }
        if file_created {
            let executable = keg.join("bin").join(&plan.bin);
            if let Err(remove_error) = fs::remove_file(&executable) {
                issues.push(format!(
                    "failed to remove '{}': {remove_error}",
                    executable.display()
                ));
            }
        }
        // `write_new_file_atomic` publishes only on success, so an error here
        // never created this receipt; never unlink a pre-existing path.
        let _ = fs::remove_dir(keg.join("bin"));
        let _ = fs::remove_dir(&keg);
        remove_created_dirs(&created);
        if issues.is_empty() {
            return Err(error);
        }
        return Err(error.context(format!(
            "tap install rollback was incomplete: {}",
            issues.join("; ")
        )));
    }

    crate::infra::store::sync_directory(&link);
    if let Some(parent) = keg.parent() {
        crate::infra::store::sync_directory(parent);
    }
    Ok(())
}

pub(super) fn ensure_taproom(
    prefix: &Path,
    plan: &TapPlan,
    created: &mut Vec<PathBuf>,
) -> Result<()> {
    for path in [
        prefix.join(TAPROOM),
        prefix.join(TAPROOM).join(&plan.owner),
        prefix.join(TAPROOM).join(&plan.owner).join(&plan.repo),
        prefix
            .join(TAPROOM)
            .join(&plan.owner)
            .join(&plan.repo)
            .join(&plan.token),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
                "refusing '{}': a tap-room path must be a real directory",
                path.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(create_error) = fs::create_dir(&path) {
                    remove_created_dirs(created);
                    return Err(create_error)
                        .with_context(|| format!("failed to create '{}'", path.display()));
                }
                created.push(path);
            }
            Err(error) => {
                remove_created_dirs(created);
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", path.display()));
            }
        }
    }
    Ok(())
}

pub(super) fn remove_created_dirs(created: &[PathBuf]) {
    for path in created.iter().rev() {
        let _ = fs::remove_dir(path);
    }
}

pub(super) fn commit_uninstall(prefix: &Path, receipt: &TapReceipt) -> Result<()> {
    verify_removal_state(prefix, receipt)?;
    let quarantine = tempfile::Builder::new()
        .prefix("tap-remove-")
        .tempdir_in(stages_dir(prefix))
        .context("failed to create a private tap removal quarantine")?;
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();

    let link_rel = Path::new(&receipt.link);
    let link = prefix.join(link_rel);
    let parked_link = quarantine.path().join("link");
    let keg_rel = Path::new(&receipt.keg);
    let keg = prefix.join(keg_rel);
    let executable_rel = keg_rel.join("bin").join(&receipt.bin);
    let executable = prefix.join(&executable_rel);
    let parked_file = quarantine.path().join("artifact");
    let parked_receipt = quarantine.path().join("receipt.json");
    let receipt_rel = receipt_relative_path(receipt)?;

    let result = (|| -> Result<()> {
        if fs::symlink_metadata(&link).is_ok() {
            if !verify_ancestor_dirs(prefix, link_rel, "tap link")? {
                bail!(
                    "refusing to park '{}': its parent directory disappeared",
                    link.display()
                );
            }
            fs::rename(&link, &parked_link)
                .with_context(|| format!("failed to park '{}'", link.display()))?;
            moved.push((link.clone(), parked_link));
        }
        if fs::symlink_metadata(&executable).is_ok() {
            if !verify_ancestor_dirs(prefix, &executable_rel, "tap executable")? {
                bail!(
                    "refusing to park '{}': its parent directory disappeared",
                    executable.display()
                );
            }
            fs::rename(&executable, &parked_file)
                .with_context(|| format!("failed to park '{}'", executable.display()))?;
            moved.push((executable.clone(), parked_file));
        }
        let source_receipt = prefix.join(&receipt_rel);
        if !verify_ancestor_dirs(prefix, &receipt_rel, "tap receipt")? {
            bail!(
                "refusing to park the tap receipt for '{}': its parent directory disappeared",
                receipt.qualified
            );
        }
        fs::rename(&source_receipt, &parked_receipt).with_context(|| {
            format!("failed to park the tap receipt for '{}'", receipt.qualified)
        })?;
        moved.push((source_receipt, parked_receipt));
        Ok(())
    })();

    if let Err(error) = result {
        let mut issues = Vec::new();
        for (source, parked) in moved.iter().rev() {
            if let Err(restore_error) = fs::rename(parked, source) {
                issues.push(format!(
                    "failed to restore '{}': {restore_error}",
                    source.display()
                ));
            }
        }
        if issues.is_empty() {
            return Err(error);
        }
        let kept = quarantine.keep();
        return Err(error.context(format!(
            "tap removal rollback was incomplete: {}; quarantine kept at '{}'",
            issues.join("; "),
            kept.display()
        )));
    }

    // Pruning is best-effort and only runs while every ancestor is still a
    // real directory below the prefix. A race leaves empty directories and the
    // quarantine instead of removing something outside the prefix.
    if verify_ancestor_dirs(prefix, &keg_rel.join("bin"), "tap keg").unwrap_or(false) {
        let _ = fs::remove_dir(keg.join("bin"));
        let _ = fs::remove_dir(&keg);
    }
    if let Some(package_rel) = keg_rel.parent()
        && verify_ancestor_dirs(prefix, package_rel, "tap package").unwrap_or(false)
    {
        let _ = fs::remove_dir(prefix.join(package_rel));
    }
    drop(quarantine);
    Ok(())
}
