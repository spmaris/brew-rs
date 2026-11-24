//! Transactional, isolated store management for native Homebrew casks.
//!
//! Cask artifacts live alongside the formula cellar under the brewrs prefix:
//!
//! ```text
//! <prefix>/.brewrs-root                       ownership marker (owned by store.rs)
//! <prefix>/.brewrs/state/casks/<token>.json   one JSON receipt per installed cask
//! <prefix>/.brewrs/state/install.lock         shared writer lock with store.rs
//! <prefix>/.brewrs/stages/                    staging and removal quarantine
//! <prefix>/Caskroom/<token>/<version>/        non-.app payload binaries & files
//! <prefix>/Applications/<Name>.app            default isolated .app bundle directory
//! <prefix>/bin/<name>                         binary symlinks and command_wrapper scripts
//! <prefix>/share/{man,zsh,fish}/...           manpages & static shell completions
//! <prefix>/etc/bash_completion.d/...          static bash completions
//! ```
//!
//! No marker file is ever written inside an `.app` bundle, preserving Apple
//! Gatekeeper code signatures. Custom `--appdir` targets are supported when
//! explicitly requested, provided they are existing non-symlink directories
//! outside protected Homebrew prefixes.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::cask_extract::{StagedCaskLink, StagedCaskPayload};
use super::store::{
    COUNTER, InstallLock, Receipt, Store, bin_dir, check_optional_directory, create_symlink,
    ensure_directory, meta_dir, monotonic_nonce, reject_protected_prefix, stages_dir, state_dir,
    write_new_file_atomic,
};
use crate::domain::cask_plan::validate_cask_identifier;
use crate::domain::validate::is_lower_hex_digest;

const CASKROOM: &str = "Caskroom";
const APPLICATIONS: &str = "Applications";
const CASKS_STATE: &str = "casks";
const MAX_VERSION_LEN: usize = 128;
const MAX_CASK_RECEIPT_BYTES: u64 = 64 * 1024;
#[cfg(unix)]
const EXDEV_ERRNO: i32 = 18;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstalledApp {
    pub bundle_name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstalledCaskLink {
    pub dest: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstalledWrapper {
    pub dest: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CaskReceipt {
    pub token: String,
    pub version: String,
    pub sha256: String,
    #[serde(default)]
    pub auto_updates: bool,
    #[serde(default)]
    pub appdir: Option<PathBuf>,
    #[serde(default)]
    pub apps: Vec<InstalledApp>,
    #[serde(default)]
    pub caskroom_files: Vec<String>,
    #[serde(default)]
    pub links: Vec<InstalledCaskLink>,
    #[serde(default)]
    pub wrappers: Vec<InstalledWrapper>,
    #[serde(default)]
    pub conflicts_with_casks: Vec<String>,
    #[serde(default)]
    pub conflicts_with_formulae: Vec<String>,
}

pub(crate) struct PreparedCask {
    pub receipt: CaskReceipt,
    pub staged: StagedCaskPayload,
    pub stage: tempfile::TempDir,
}

/// Resolve and validate the target `--appdir` directory without creating
/// external paths.
///
/// The default is `<prefix>/Applications`. An explicit `--appdir` must be an
/// existing absolute non-symlink directory outside protected Homebrew prefixes.
pub(crate) fn resolve_appdir(prefix: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        None => {
            let default = applications_dir(prefix);
            check_optional_directory(&default)?;
            Ok(default)
        }
        Some(dir) => {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                bail!(
                    "refusing --appdir '{}': use an absolute directory path",
                    dir.display()
                );
            }
            reject_protected_prefix(dir)?;
            let meta = fs::symlink_metadata(dir).with_context(|| {
                format!(
                    "refusing --appdir '{}': directory does not exist or cannot be inspected",
                    dir.display()
                )
            })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!(
                    "refusing --appdir '{}': must be a real directory, not a symlink",
                    dir.display()
                );
            }
            let canonical = fs::canonicalize(dir)
                .with_context(|| format!("failed to resolve --appdir '{}'", dir.display()))?;
            reject_protected_prefix(&canonical)?;
            Ok(dir.to_path_buf())
        }
    }
}

/// Read every installed native cask receipt, sorted by token.
pub(crate) fn installed(store: &Store) -> Result<Vec<CaskReceipt>> {
    if !store.is_owned() {
        return Ok(Vec::new());
    }
    let prefix = store.prefix();
    if !check_optional_directory(&meta_dir(prefix))?
        || !check_optional_directory(&state_dir(prefix))?
        || !check_optional_directory(&casks_state_dir(prefix))?
    {
        return Ok(Vec::new());
    }

    let dir = casks_state_dir(prefix);
    let mut receipts = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect '{}'", dir.display()))?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(".json") else {
            continue;
        };
        let receipt = read_cask_receipt(&entry.path())?;
        if receipt.token != stem {
            bail!(
                "cask receipt '{}' names '{}', not '{}'",
                entry.path().display(),
                receipt.token,
                stem
            );
        }
        receipts.push(receipt);
    }
    receipts.sort_by(|left, right| left.token.cmp(&right.token));
    Ok(receipts)
}

/// Reject any projected cask+formula set with declared cask or formula conflicts.
pub(crate) fn validate_cask_conflicts(casks: &[CaskReceipt], formulae: &[Receipt]) -> Result<()> {
    let cask_tokens: HashSet<&str> = casks.iter().map(|c| c.token.as_str()).collect();
    let formula_names: HashSet<&str> = formulae.iter().map(|f| f.name.as_str()).collect();
    for cask in casks {
        for conflict in &cask.conflicts_with_casks {
            if cask_tokens.contains(conflict.as_str()) {
                bail!(
                    "cask '{}' conflicts with '{}'; remove '{}' before installing '{}'",
                    cask.token,
                    conflict,
                    conflict,
                    cask.token
                );
            }
        }
        for conflict in &cask.conflicts_with_formulae {
            if formula_names.contains(conflict.as_str()) {
                bail!(
                    "cask '{}' conflicts with formula '{}'; remove '{}' before installing '{}'",
                    cask.token,
                    conflict,
                    conflict,
                    cask.token
                );
            }
        }
    }
    Ok(())
}

/// Atomically install prepared casks into `store` under its writer lock.
pub(crate) fn commit(store: &Store, packages: Vec<PreparedCask>) -> Result<()> {
    store.ensure_owned()?;
    if packages.is_empty() {
        return Ok(());
    }
    let _lock = InstallLock::acquire(store.prefix())?;
    let existing_casks = installed(store)?;
    let existing_formulae = store.installed()?;
    let pending = prepare_commit(store, packages, &existing_casks, &existing_formulae)?;
    let mut applied = Vec::new();
    match apply_commit(store, pending, &mut applied, false) {
        Ok(()) => Ok(()),
        Err(error) => finish_commit(error, rollback_applied(&applied)),
    }
}

/// Atomically remove exact observed cask receipts under `store`'s writer lock.
pub(crate) fn remove(store: &Store, expected: &[CaskReceipt]) -> Result<()> {
    store.verify_owned()?;
    if expected.is_empty() {
        return Ok(());
    }
    let _lock = InstallLock::acquire(store.prefix())?;
    let removals = plan_cask_removals(store, expected)?;
    let quarantine = tempfile::Builder::new()
        .prefix("cask-remove-")
        .tempdir_in(stages_dir(store.prefix()))
        .context("failed to create cask removal quarantine")?;

    let mut parked = Vec::new();
    match apply_removals(&removals, quarantine.path(), &mut parked) {
        Ok(()) => {
            cleanup_parked_siblings(&parked);
            drop(quarantine);
            Ok(())
        }
        Err(error) => {
            let mut issues = rollback_parked(&parked);
            let msg = if issues.is_empty() {
                drop(quarantine);
                return Err(error);
            } else {
                let kept = quarantine.keep();
                issues.push(format!("quarantine kept at '{}'", kept.display()));
                issues.join("; ")
            };
            Err(error.context(format!("cask removal rollback was incomplete: {msg}")))
        }
    }
}

/// Atomically upgrade observed cask receipts to their replacements under one
/// writer lock, restoring old casks on failure.
pub(crate) fn upgrade(store: &Store, replacements: Vec<(CaskReceipt, PreparedCask)>) -> Result<()> {
    store.ensure_owned()?;
    if replacements.is_empty() {
        return Ok(());
    }
    let _lock = InstallLock::acquire(store.prefix())?;

    let mut olds = Vec::with_capacity(replacements.len());
    let mut prepared_news = Vec::with_capacity(replacements.len());
    let mut seen = HashSet::new();
    for (old, new) in replacements {
        if old.token != new.receipt.token {
            bail!(
                "refusing cask upgrade from '{}' to mismatched token '{}'",
                old.token,
                new.receipt.token
            );
        }
        if old.version == new.receipt.version && old.sha256 == new.receipt.sha256 {
            bail!(
                "refusing to upgrade cask '{}' in place at identical version '{}' and sha256",
                old.token,
                old.version
            );
        }
        if !seen.insert(old.token.clone()) {
            bail!("cask '{}' appears more than once in one upgrade", old.token);
        }
        olds.push(old);
        prepared_news.push(new);
    }

    let existing_casks = installed(store)?;
    let existing_formulae = store.installed()?;
    let retained_casks: Vec<CaskReceipt> = existing_casks
        .into_iter()
        .filter(|entry| !seen.contains(&entry.token))
        .collect();

    let removals = plan_cask_removals(store, &olds)?;
    let mut projected = retained_casks.clone();
    for item in &prepared_news {
        validate_cask_receipt(&item.receipt)?;
        projected.push(item.receipt.clone());
    }
    validate_cask_conflicts(&projected, &existing_formulae)?;

    let quarantine = tempfile::Builder::new()
        .prefix("cask-upgrade-")
        .tempdir_in(stages_dir(store.prefix()))
        .context("failed to create cask upgrade quarantine")?;
    let mut parked = Vec::new();
    if let Err(error) = apply_removals(&removals, quarantine.path(), &mut parked) {
        let issues = rollback_parked(&parked);
        return if issues.is_empty() {
            drop(quarantine);
            Err(error)
        } else {
            let kept = quarantine.keep();
            Err(error.context(format!(
                "upgrade preflight rollback failed: {}; quarantine kept at '{}'",
                issues.join("; "),
                kept.display()
            )))
        };
    }

    let pending = match prepare_commit(store, prepared_news, &retained_casks, &existing_formulae) {
        Ok(pending) => pending,
        Err(error) => {
            let issues = rollback_parked(&parked);
            return finish_upgrade_err(quarantine, issues, error);
        }
    };

    let mut applied = Vec::new();
    match apply_commit(store, pending, &mut applied, true) {
        Ok(()) => {
            cleanup_parked_siblings(&parked);
            drop(quarantine);
            Ok(())
        }
        Err(error) => {
            let mut issues = rollback_applied(&applied);
            issues.extend(rollback_parked(&parked));
            finish_upgrade_err(quarantine, issues, error)
        }
    }
}

struct PendingCask {
    receipt: CaskReceipt,
    staged: StagedCaskPayload,
    stage: tempfile::TempDir,
}

fn prepare_commit(
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

struct AppliedCask {
    receipt_path: PathBuf,
    links: Vec<PathBuf>,
    caskroom_pkg: PathBuf,
    caskroom_pkg_created: bool,
    caskroom_ver: Option<PathBuf>,
    apps: Vec<PathBuf>,
}

fn apply_commit(
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
        let record = applied.last_mut().expect("just pushed");

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

fn rollback_applied(applied: &[AppliedCask]) -> Vec<String> {
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

fn finish_commit(error: anyhow::Error, issues: Vec<String>) -> Result<()> {
    if issues.is_empty() {
        Err(error)
    } else {
        Err(error.context(format!(
            "cask rollback was incomplete: {}",
            issues.join("; ")
        )))
    }
}

fn finish_upgrade_err(
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

struct CaskRemovalPlan {
    steps: Vec<CaskRemovalStep>,
    prune_pkg: Option<PathBuf>,
}

enum CaskRemovalStep {
    Receipt(PathBuf),
    Link {
        path: PathBuf,
        expected_target: String,
    },
    Wrapper {
        path: PathBuf,
        expected_sha256: String,
    },
    CaskroomVersionDir(PathBuf),
    AppDir(PathBuf),
}

enum ParkedEntry {
    Quarantined { source: PathBuf, parked: PathBuf },
    CrossDeviceSibling { source: PathBuf, sibling: PathBuf },
    PrunedDir(PathBuf),
}

fn plan_cask_removals(store: &Store, expected: &[CaskReceipt]) -> Result<Vec<CaskRemovalPlan>> {
    let prefix = store.prefix();
    let state_dir_path = casks_state_dir(prefix);
    let mut seen = HashSet::new();
    let mut plans = Vec::with_capacity(expected.len());

    for receipt in expected {
        validate_cask_receipt(receipt)?;
        if !seen.insert(receipt.token.as_str()) {
            bail!(
                "cask '{}' appears more than once in removal list",
                receipt.token
            );
        }

        let receipt_path = state_dir_path.join(format!("{}.json", receipt.token));
        let current = read_cask_receipt(&receipt_path).with_context(|| {
            format!(
                "refusing to remove cask '{} {}': receipt '{}' could not be read",
                receipt.token,
                receipt.version,
                receipt_path.display()
            )
        })?;
        if current != *receipt {
            bail!(
                "refusing to remove cask '{} {}': receipt changed on disk",
                receipt.token,
                receipt.version
            );
        }

        let mut steps = vec![CaskRemovalStep::Receipt(receipt_path)];

        for link in &receipt.links {
            let link_path = safe_prefix_join_existing_no_symlinks(prefix, &link.dest)?;
            let meta = fs::symlink_metadata(&link_path).with_context(|| {
                format!(
                    "refusing to remove cask '{}': owned symlink '{}' is missing",
                    receipt.token,
                    link_path.display()
                )
            })?;
            if !meta.file_type().is_symlink() {
                bail!(
                    "refusing to remove cask '{}': '{}' is no longer a symlink",
                    receipt.token,
                    link_path.display()
                );
            }
            let actual_target = fs::read_link(&link_path).with_context(|| {
                format!("failed to read cask symlink '{}'", link_path.display())
            })?;
            if actual_target != Path::new(&link.target) {
                bail!(
                    "refusing to remove cask '{}': symlink '{}' points to '{}' instead of '{}'",
                    receipt.token,
                    link_path.display(),
                    actual_target.display(),
                    link.target
                );
            }
            steps.push(CaskRemovalStep::Link {
                path: link_path,
                expected_target: link.target.clone(),
            });
        }

        for wrapper in &receipt.wrappers {
            let wrapper_path = safe_prefix_join_existing_no_symlinks(prefix, &wrapper.dest)?;
            verify_regular_file_sha256(&wrapper_path, &wrapper.sha256).with_context(|| {
                format!(
                    "refusing to remove cask '{}': command_wrapper '{}' was modified or removed",
                    receipt.token,
                    wrapper_path.display()
                )
            })?;
            steps.push(CaskRemovalStep::Wrapper {
                path: wrapper_path,
                expected_sha256: wrapper.sha256.clone(),
            });
        }

        for app in &receipt.apps {
            let Some(parent) = app.path.parent() else {
                bail!("invalid app path '{}' in receipt", app.path.display());
            };
            let parent_meta = fs::symlink_metadata(parent)
                .with_context(|| format!("failed to inspect appdir '{}'", parent.display()))?;
            if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
                bail!(
                    "appdir '{}' is no longer a real directory",
                    parent.display()
                );
            }
            let app_meta = fs::symlink_metadata(&app.path).with_context(|| {
                format!(
                    "refusing to remove cask '{}': app bundle '{}' is missing",
                    receipt.token,
                    app.path.display()
                )
            })?;
            if app_meta.file_type().is_symlink() || !app_meta.is_dir() {
                bail!(
                    "refusing to remove cask '{}': '{}' is not a real .app directory",
                    receipt.token,
                    app.path.display()
                );
            }
            steps.push(CaskRemovalStep::AppDir(app.path.clone()));
        }

        let pkg_dir = caskroom_dir(prefix).join(&receipt.token);
        let mut prune_pkg = None;
        if !receipt.caskroom_files.is_empty() {
            let ver_dir = pkg_dir.join(&receipt.version);
            let meta = fs::symlink_metadata(&ver_dir).with_context(|| {
                format!(
                    "refusing to remove cask '{}': Caskroom version '{}' is missing",
                    receipt.token,
                    ver_dir.display()
                )
            })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                bail!(
                    "refusing to remove cask '{}': '{}' is not a real directory",
                    receipt.token,
                    ver_dir.display()
                );
            }
            steps.push(CaskRemovalStep::CaskroomVersionDir(ver_dir));
            prune_pkg = Some(pkg_dir);
        }

        plans.push(CaskRemovalPlan { steps, prune_pkg });
    }
    Ok(plans)
}

fn apply_removals(
    plans: &[CaskRemovalPlan],
    quarantine: &Path,
    parked: &mut Vec<ParkedEntry>,
) -> Result<()> {
    for plan in plans {
        for step in &plan.steps {
            match step {
                CaskRemovalStep::Receipt(path) => {
                    park_same_fs_or_sibling(path, quarantine, parked, false)?;
                }
                CaskRemovalStep::Link {
                    path,
                    expected_target,
                } => {
                    let meta = fs::symlink_metadata(path)?;
                    if !meta.file_type().is_symlink()
                        || fs::read_link(path)? != Path::new(expected_target)
                    {
                        bail!("cask symlink '{}' changed before removal", path.display());
                    }
                    park_same_fs_or_sibling(path, quarantine, parked, false)?;
                }
                CaskRemovalStep::Wrapper {
                    path,
                    expected_sha256,
                } => {
                    verify_regular_file_sha256(path, expected_sha256)?;
                    park_same_fs_or_sibling(path, quarantine, parked, false)?;
                }
                CaskRemovalStep::CaskroomVersionDir(path) | CaskRemovalStep::AppDir(path) => {
                    let meta = fs::symlink_metadata(path)?;
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        bail!(
                            "cask directory '{}' is no longer a real directory",
                            path.display()
                        );
                    }
                    park_same_fs_or_sibling(path, quarantine, parked, true)?;
                }
            }
        }
        if let Some(ref pkg_dir) = plan.prune_pkg
            && let Ok(()) = fs::remove_dir(pkg_dir)
        {
            parked.push(ParkedEntry::PrunedDir(pkg_dir.clone()));
        }
    }
    Ok(())
}

fn park_same_fs_or_sibling(
    source: &Path,
    quarantine: &Path,
    parked: &mut Vec<ParkedEntry>,
    allow_cross_device_sibling: bool,
) -> Result<()> {
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let qdest = quarantine.join(format!("item-{seq}"));
    match fs::rename(source, &qdest) {
        Ok(()) => {
            parked.push(ParkedEntry::Quarantined {
                source: source.to_path_buf(),
                parked: qdest,
            });
            Ok(())
        }
        Err(err) if allow_cross_device_sibling && is_cross_device(&err) => {
            let parent = source
                .parent()
                .context("app bundle path has no parent directory")?;
            let sibling = parent.join(format!(
                ".brewrs-parked-{}-{}-{seq}",
                std::process::id(),
                monotonic_nonce()
            ));
            fs::rename(source, &sibling).with_context(|| {
                format!(
                    "failed to move '{}' to temporary removal path '{}'",
                    source.display(),
                    sibling.display()
                )
            })?;
            parked.push(ParkedEntry::CrossDeviceSibling {
                source: source.to_path_buf(),
                sibling,
            });
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to quarantine '{}' during cask removal",
                source.display()
            )
        }),
    }
}

fn rollback_parked(parked: &[ParkedEntry]) -> Vec<String> {
    let mut issues = Vec::new();
    for entry in parked.iter().rev() {
        match entry {
            ParkedEntry::Quarantined { source, parked } => {
                if let Err(err) = fs::rename(parked, source) {
                    issues.push(format!("failed to restore '{}': {err}", source.display()));
                }
            }
            ParkedEntry::CrossDeviceSibling { source, sibling } => {
                if let Err(err) = fs::rename(sibling, source) {
                    issues.push(format!("failed to restore '{}': {err}", source.display()));
                }
            }
            ParkedEntry::PrunedDir(dir) => {
                if let Err(err) = fs::create_dir(dir) {
                    issues.push(format!("failed to recreate '{}': {err}", dir.display()));
                }
            }
        }
    }
    issues
}

fn cleanup_parked_siblings(parked: &[ParkedEntry]) {
    for entry in parked {
        if let ParkedEntry::CrossDeviceSibling { sibling, .. } = entry {
            let _ = fs::remove_dir_all(sibling);
        }
    }
}

fn install_app_dir(src: &Path, dst: &Path) -> Result<()> {
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

fn is_cross_device(err: &io::Error) -> bool {
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

fn validate_stage_ownership(prefix: &Path, stage: &Path) -> Result<()> {
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

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("refusing to overwrite existing path '{}'", path.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

fn ensure_prefix_dest_absent(prefix: &Path, rel: &str) -> Result<()> {
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

fn safe_prefix_join_and_mkdirs(prefix: &Path, rel: &str) -> Result<PathBuf> {
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

fn safe_prefix_join_existing_no_symlinks(prefix: &Path, rel: &str) -> Result<PathBuf> {
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

fn verify_regular_file_sha256(path: &Path, expected: &str) -> Result<()> {
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

fn write_cask_receipt(directory: &Path, receipt: &CaskReceipt) -> Result<()> {
    let target = directory.join(format!("{}.json", receipt.token));
    let mut bytes = serde_json::to_vec(receipt)
        .with_context(|| format!("failed to encode cask receipt '{}'", receipt.token))?;
    bytes.push(b'\n');
    write_new_file_atomic(directory, &target, &bytes)
}

fn read_cask_receipt(path: &Path) -> Result<CaskReceipt> {
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

fn validate_cask_receipt(receipt: &CaskReceipt) -> Result<()> {
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

pub(super) fn casks_state_dir(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(CASKS_STATE)
}

pub(super) fn caskroom_dir(prefix: &Path) -> PathBuf {
    prefix.join(CASKROOM)
}

pub(super) fn applications_dir(prefix: &Path) -> PathBuf {
    prefix.join(APPLICATIONS)
}

#[cfg(test)]
mod tests;
