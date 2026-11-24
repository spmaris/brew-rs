//! Preparing, applying, and rolling back cask commits.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::support::{
    ensure_absent, ensure_prefix_dest_absent, install_app_dir, safe_prefix_join_and_mkdirs,
    validate_cask_receipt, validate_stage_ownership, write_cask_receipt,
};
use super::{
    AppliedCask, CaskReceipt, PendingCask, PreparedCask, applications_dir, caskroom_dir,
    casks_state_dir, resolve_appdir, validate_cask_conflicts,
};
use crate::infra::cask_extract::StagedCaskLink;
use crate::infra::store::{
    Receipt, Store, bin_dir, check_optional_directory, create_symlink, ensure_directory,
};

pub(super) fn prepare_commit(
    store: &Store,
    packages: Vec<PreparedCask>,
    existing_casks: &[CaskReceipt],
    existing_formulae: &[Receipt],
) -> Result<Vec<PendingCask>> {
    let prefix = store.prefix();
    let existing_tokens: HashSet<&str> = existing_casks.iter().map(|c| c.token.as_str()).collect();
    let mut batch_tokens = HashSet::new();
    let mut batch_apps = HashSet::new();
    let mut batch_dests = HashSet::new();
    let mut projected = existing_casks.to_vec();
    let mut pending = Vec::with_capacity(packages.len());

    check_optional_directory(&casks_state_dir(prefix))?;
    check_optional_directory(&caskroom_dir(prefix))?;
    check_optional_directory(&applications_dir(prefix))?;
    check_optional_directory(&bin_dir(prefix))?;

    for PreparedCask {
        receipt,
        staged,
        stage,
    } in packages
    {
        validate_cask_receipt(&receipt)?;
        validate_stage_ownership(prefix, stage.path())?;
        if !batch_tokens.insert(receipt.token.clone()) {
            bail!(
                "cask '{}' appears more than once in one commit",
                receipt.token
            );
        }
        if existing_tokens.contains(receipt.token.as_str()) {
            bail!("cask '{}' is already installed", receipt.token);
        }

        // Check appdir validity when receipt specifies one.
        if let Some(ref appdir) = receipt.appdir {
            if appdir != &applications_dir(prefix) {
                resolve_appdir(prefix, Some(appdir))?;
            } else {
                check_optional_directory(appdir)?;
            }
        }

        // Ensure no Caskroom version dir or receipt already exists.
        ensure_absent(&casks_state_dir(prefix).join(format!("{}.json", receipt.token)))?;
        if staged.has_caskroom_files {
            ensure_absent(
                &caskroom_dir(prefix)
                    .join(&receipt.token)
                    .join(&receipt.version),
            )?;
        }

        // Ensure every `.app` destination is absent and uncolliding.
        for app in &receipt.apps {
            if !batch_apps.insert(app.path.clone()) {
                bail!(
                    "duplicate .app destination '{}' in one commit",
                    app.path.display()
                );
            }
            ensure_absent(&app.path)?;
        }

        // Ensure every prefix symlink & wrapper destination is absent and safe.
        for link in &receipt.links {
            if !batch_dests.insert(link.dest.clone()) {
                bail!("duplicate cask destination '{}' in one commit", link.dest);
            }
            ensure_prefix_dest_absent(prefix, &link.dest)?;
        }
        for wrapper in &receipt.wrappers {
            if !batch_dests.insert(wrapper.dest.clone()) {
                bail!(
                    "duplicate cask destination '{}' in one commit",
                    wrapper.dest
                );
            }
            ensure_prefix_dest_absent(prefix, &wrapper.dest)?;
        }

        projected.push(receipt.clone());
        pending.push(PendingCask {
            receipt,
            staged,
            stage,
        });
    }

    validate_cask_conflicts(&projected, existing_formulae)?;
    Ok(pending)
}

pub(super) fn apply_commit(
    store: &Store,
    pending: Vec<PendingCask>,
    applied: &mut Vec<AppliedCask>,
    allow_existing_caskroom_pkg: bool,
) -> Result<()> {
    let prefix = store.prefix();
    let state_dir_path = casks_state_dir(prefix);
    let caskroom = caskroom_dir(prefix);
    let default_apps = applications_dir(prefix);

    ensure_directory(&state_dir_path, true)?;

    for item in pending {
        let token = item.receipt.token.clone();
        let version = item.receipt.version.clone();
        let pkg_dir = caskroom.join(&token);
        let pkg_created = if item.staged.has_caskroom_files {
            ensure_directory(&caskroom, true)?;
            match fs::symlink_metadata(&pkg_dir) {
                Ok(meta) => {
                    if !allow_existing_caskroom_pkg
                        || meta.file_type().is_symlink()
                        || !meta.is_dir()
                    {
                        bail!(
                            "unexpected existing Caskroom directory '{}'",
                            pkg_dir.display()
                        );
                    }
                    false
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&pkg_dir).with_context(|| {
                        format!(
                            "failed to create Caskroom directory '{}'",
                            pkg_dir.display()
                        )
                    })?;
                    true
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("failed to inspect '{}'", pkg_dir.display()));
                }
            }
        } else {
            false
        };

        applied.push(AppliedCask {
            receipt_path: state_dir_path.join(format!("{token}.json")),
            links: Vec::new(),
            caskroom_pkg: pkg_dir.clone(),
            caskroom_pkg_created: pkg_created,
            caskroom_ver: None,
            apps: Vec::new(),
        });
        let record = applied
            .last_mut()
            .context("internal error: applied cask record missing after push")?;

        if item.staged.has_caskroom_files {
            let staged_caskroom = item.stage.path().join("caskroom");
            let ver_dir = pkg_dir.join(&version);
            fs::rename(&staged_caskroom, &ver_dir).with_context(|| {
                format!(
                    "failed to move staged Caskroom files into '{}'",
                    ver_dir.display()
                )
            })?;
            record.caskroom_ver = Some(ver_dir);
        }

        // Move staged `.app` bundles into appdir (creating prefix/Applications lazily).
        for staged_app in &item.staged.app_bundles {
            let dest_app = item
                .receipt
                .apps
                .iter()
                .find(|a| a.bundle_name == staged_app.bundle_name)
                .context("staged app bundle missing from cask receipt")?
                .path
                .clone();
            if dest_app.parent() == Some(default_apps.as_path()) {
                ensure_directory(&default_apps, true)?;
            }
            install_app_dir(&staged_app.staged_dir, &dest_app)?;
            record.apps.push(dest_app);
        }

        for StagedCaskLink {
            relative_dest,
            target,
        } in &item.staged.symlinks
        {
            let link_path = safe_prefix_join_and_mkdirs(prefix, relative_dest)?;
            create_symlink(Path::new(target), &link_path)
                .with_context(|| format!("failed to create cask link '{}'", link_path.display()))?;
            record.links.push(link_path);
        }

        for wrapper in &item.staged.wrappers {
            let dest_path = safe_prefix_join_and_mkdirs(prefix, &wrapper.relative_dest)?;
            fs::rename(&wrapper.staged_file, &dest_path).with_context(|| {
                format!(
                    "failed to install command wrapper '{}'",
                    dest_path.display()
                )
            })?;
            record.links.push(dest_path);
        }

        write_cask_receipt(&state_dir_path, &item.receipt)?;
        drop(item.stage);
    }

    Ok(())
}

pub(super) fn rollback_applied(applied: &[AppliedCask]) -> Vec<String> {
    let mut issues = Vec::new();
    for record in applied.iter().rev() {
        match fs::symlink_metadata(&record.receipt_path) {
            Ok(meta) if !meta.file_type().is_symlink() && meta.is_file() => {
                if let Err(err) = fs::remove_file(&record.receipt_path) {
                    issues.push(format!(
                        "failed to remove '{}': {err}",
                        record.receipt_path.display()
                    ));
                    continue;
                }
            }
            Ok(_) => {
                issues.push(format!(
                    "left '{}': not a regular receipt",
                    record.receipt_path.display()
                ));
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                issues.push(format!(
                    "failed to inspect '{}': {e}",
                    record.receipt_path.display()
                ));
                continue;
            }
        }

        for path in record.links.iter().rev() {
            match fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() || meta.is_file() => {
                    if let Err(err) = fs::remove_file(path) {
                        issues.push(format!("failed to remove '{}': {err}", path.display()));
                    }
                }
                Ok(_) => issues.push(format!("left unexpected entry '{}'", path.display())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => issues.push(format!("failed to inspect '{}': {e}", path.display())),
            }
        }

        for app_path in record.apps.iter().rev() {
            match fs::symlink_metadata(app_path) {
                Ok(meta) if !meta.file_type().is_symlink() && meta.is_dir() => {
                    if let Err(err) = fs::remove_dir_all(app_path) {
                        issues.push(format!("failed to remove '{}': {err}", app_path.display()));
                    }
                }
                Ok(_) => issues.push(format!("left unexpected entry '{}'", app_path.display())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => issues.push(format!("failed to inspect '{}': {e}", app_path.display())),
            }
        }

        if let Some(ref ver) = record.caskroom_ver
            && ver.exists()
            && let Err(err) = fs::remove_dir_all(ver)
        {
            issues.push(format!("failed to remove '{}': {err}", ver.display()));
        }
        if record.caskroom_pkg_created {
            let _ = fs::remove_dir(&record.caskroom_pkg);
        }
    }
    issues
}

pub(super) fn finish_commit(error: anyhow::Error, issues: Vec<String>) -> Result<()> {
    if issues.is_empty() {
        Err(error)
    } else {
        Err(error.context(format!(
            "cask rollback was incomplete: {}",
            issues.join("; ")
        )))
    }
}

pub(super) fn finish_upgrade_err(
    quarantine: tempfile::TempDir,
    issues: Vec<String>,
    error: anyhow::Error,
) -> Result<()> {
    if issues.is_empty() {
        drop(quarantine);
        Err(error)
    } else {
        let kept = quarantine.keep();
        Err(error.context(format!(
            "cask upgrade rollback was incomplete: {}; quarantine kept at '{}'",
            issues.join("; "),
            kept.display()
        )))
    }
}
