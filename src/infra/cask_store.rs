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

mod error;
mod install;
mod removal;
mod support;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) use self::error::CaskStoreError;
use self::install::{
    apply_commit, finish_commit, finish_upgrade_err, prepare_commit, rollback_applied,
};
use self::removal::{apply_removals, cleanup_parked_siblings, plan_cask_removals, rollback_parked};
use self::support::{read_cask_receipt, validate_cask_receipt};
use super::cask_extract::StagedCaskPayload;
use super::store::{
    InstallLock, Receipt, Store, check_optional_directory, meta_dir, reject_protected_prefix,
    stages_dir, state_dir,
};

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

struct PendingCask {
    receipt: CaskReceipt,
    staged: StagedCaskPayload,
    stage: tempfile::TempDir,
}

struct AppliedCask {
    receipt_path: PathBuf,
    links: Vec<PathBuf>,
    caskroom_pkg: PathBuf,
    caskroom_pkg_created: bool,
    caskroom_ver: Option<PathBuf>,
    apps: Vec<PathBuf>,
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

/// Resolve and validate the target `--appdir` directory without creating
/// external paths.
///
/// The default is `<prefix>/Applications`. An explicit `--appdir` must be an
/// existing absolute non-symlink directory outside protected Homebrew prefixes.
pub(crate) fn resolve_appdir(
    prefix: &Path,
    explicit: Option<&Path>,
) -> Result<PathBuf, CaskStoreError> {
    match explicit {
        None => {
            let default = applications_dir(prefix);
            check_optional_directory(&default)?;
            Ok(default)
        }
        Some(dir) => {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                return Err(CaskStoreError::AppdirNotAbsolute {
                    path: dir.to_path_buf(),
                });
            }
            reject_protected_prefix(dir)?;
            let meta =
                fs::symlink_metadata(dir).map_err(|source| CaskStoreError::AppdirInaccessible {
                    path: dir.to_path_buf(),
                    source,
                })?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(CaskStoreError::AppdirNotRealDirectory {
                    path: dir.to_path_buf(),
                });
            }
            let canonical = fs::canonicalize(dir)
                .map_err(CaskStoreError::io("failed to resolve --appdir", &dir))?;
            reject_protected_prefix(&canonical)?;
            Ok(dir.to_path_buf())
        }
    }
}

/// Read every installed native cask receipt, sorted by token.
pub(crate) fn installed(store: &Store) -> Result<Vec<CaskReceipt>, CaskStoreError> {
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
    for entry in fs::read_dir(&dir).map_err(CaskStoreError::io("failed to read", &dir))? {
        let entry = entry.map_err(CaskStoreError::io("failed to inspect", &dir))?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = file_name.strip_suffix(".json") else {
            continue;
        };
        let receipt = read_cask_receipt(&entry.path())?;
        if receipt.token != stem {
            return Err(CaskStoreError::ReceiptNameMismatch {
                path: entry.path().to_path_buf(),
                actual: receipt.token.to_string(),
                expected: stem.to_string(),
            });
        }
        receipts.push(receipt);
    }
    receipts.sort_by(|left, right| left.token.cmp(&right.token));
    Ok(receipts)
}

/// Reject any projected cask+formula set with declared cask or formula conflicts.
pub(crate) fn validate_cask_conflicts(
    casks: &[CaskReceipt],
    formulae: &[Receipt],
) -> Result<(), CaskStoreError> {
    let cask_tokens: HashSet<&str> = casks.iter().map(|c| c.token.as_str()).collect();
    let formula_names: HashSet<&str> = formulae.iter().map(|f| f.name.as_str()).collect();
    for cask in casks {
        for conflict in &cask.conflicts_with_casks {
            if cask_tokens.contains(conflict.as_str()) {
                return Err(CaskStoreError::CaskConflict {
                    token: cask.token.to_string(),
                    conflict: conflict.to_string(),
                });
            }
        }
        for conflict in &cask.conflicts_with_formulae {
            if formula_names.contains(conflict.as_str()) {
                return Err(CaskStoreError::FormulaConflict {
                    token: cask.token.to_string(),
                    conflict: conflict.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Atomically install prepared casks into `store` under its writer lock.
pub(crate) fn commit(store: &Store, packages: Vec<PreparedCask>) -> Result<(), CaskStoreError> {
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
pub(crate) fn remove(store: &Store, expected: &[CaskReceipt]) -> Result<(), CaskStoreError> {
    store.verify_owned()?;
    if expected.is_empty() {
        return Ok(());
    }
    let _lock = InstallLock::acquire(store.prefix())?;
    let removals = plan_cask_removals(store, expected)?;
    let quarantine = tempfile::Builder::new()
        .prefix("cask-remove-")
        .tempdir_in(stages_dir(store.prefix()))
        .map_err(|source| CaskStoreError::RemovalQuarantine { source })?;

    let mut parked = Vec::new();
    match apply_removals(&removals, quarantine.path(), &mut parked) {
        Ok(()) => {
            cleanup_parked_siblings(&parked);
            drop(quarantine);
            Ok(())
        }
        Err(error) => {
            let mut issues = rollback_parked(&parked);
            if issues.is_empty() {
                drop(quarantine);
                return Err(error);
            }
            let kept = quarantine.keep();
            issues.push(format!("quarantine kept at '{}'", kept.display()));
            Err(CaskStoreError::RemovalRollbackIncomplete {
                issues,
                source: Box::new(error),
            })
        }
    }
}

/// Atomically upgrade observed cask receipts to their replacements under one
/// writer lock, restoring old casks on failure.
pub(crate) fn upgrade(
    store: &Store,
    replacements: Vec<(CaskReceipt, PreparedCask)>,
) -> Result<(), CaskStoreError> {
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
            return Err(CaskStoreError::UpgradeTokenMismatch {
                from: old.token.to_string(),
                to: new.receipt.token.to_string(),
            });
        }
        if old.version == new.receipt.version && old.sha256 == new.receipt.sha256 {
            return Err(CaskStoreError::UpgradeIdentical {
                token: old.token.to_string(),
                version: old.version.to_string(),
            });
        }
        if !seen.insert(old.token.clone()) {
            return Err(CaskStoreError::DuplicateInUpgrade {
                token: old.token.to_string(),
            });
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
        .map_err(|source| CaskStoreError::UpgradeQuarantine { source })?;
    let mut parked = Vec::new();
    if let Err(error) = apply_removals(&removals, quarantine.path(), &mut parked) {
        let issues = rollback_parked(&parked);
        return if issues.is_empty() {
            drop(quarantine);
            Err(error)
        } else {
            let kept = quarantine.keep();
            Err(CaskStoreError::UpgradePreflightRollbackFailed {
                issues,
                kept,
                source: Box::new(error),
            })
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
