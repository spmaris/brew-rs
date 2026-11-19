//! A dedicated, Homebrew-independent prefix for portable bottles.
//!
//! The store owns a small self-contained prefix:
//!
//! ```text
//! <prefix>/.brewrs-root                 ownership marker
//! <prefix>/.brewrs/state/receipts/      one JSON receipt per package
//! <prefix>/.brewrs/state/install.lock   writer lock
//! <prefix>/.brewrs/stages/              private staging directories
//! <prefix>/Cellar/<name>/<version>/     keg contents (a staged root, renamed)
//! <prefix>/bin/<member>                 relative links into the keg
//! ```
//!
//! Nothing here writes into a Homebrew prefix. A root is only usable when it
//! carries the marker, so an existing unrelated directory is never adopted by
//! accident, and known Homebrew prefixes are rejected after canonicalizing the
//! deepest existing ancestor (a symlinked path cannot smuggle a store into one).
//!
//! # Commit atomicity
//!
//! Each package is applied as: create `Cellar/<name>`, rename the staged root
//! to `Cellar/<name>/<version>`, create the `bin` links, then write the receipt
//! last. On failure everything created by this commit is rolled back in reverse
//! order. The lock serializes brewrs writers, so preflight collisions are not
//! expected to race with another commit.
//!
//! Commits are not crash-atomic. A process death after the rename but before
//! the final receipt leaves an unreceipted keg (and possibly links) behind.
//! [`Store::installed`] only trusts receipts, so such a package is not reported
//! installed, and a later commit refuses the leftover path with a message to
//! remove it. Receipts are never written before their files and links exist.
//!
//! # Removal atomicity
//!
//! [`Store::remove`] accepts the exact receipts a caller observed, rechecks
//! them under the same writer lock as a commit, and preflights every selected
//! receipt, keg, and `bin` link before moving anything. Each path is renamed
//! into a private quarantine on the same filesystem, and every applied move is
//! reversed if a later one fails. Foreign data is never overwritten, removed,
//! or followed: a `bin` entry is removed only when it is a symlink whose stored
//! target is exactly the relative link a commit created for that package and
//! version, only the receipt's exact keg is moved, and an emptied
//! `Cellar/<name>` is pruned with a non-recursive removal that keeps any
//! foreign entry.
//!
//! Removal is not crash-atomic either. The receipt moves first, so a process
//! death mid-removal leaves a keg and possibly links without a receipt, plus a
//! `remove-*` quarantine below `.brewrs/stages`: the package is no longer
//! reported installed, a later commit refuses the leftover path, and re-running
//! removal refuses the missing receipt. Those leftovers need manual cleanup.
//!
//! # Upgrade atomicity
//!
//! [`Store::upgrade`] takes the exact old receipts a preview observed and fresh
//! stages for their replacements. Under one writer lock it rechecks every old
//! receipt, keg, and exact owned `bin` link, rejects duplicate names, a changed
//! requested flag, a same-version replacement, alternate versions, and foreign
//! `bin` or package entries, then moves only the old receipt, keg, and owned
//! links into a private quarantine before installing the new packages. A failed
//! install rolls the new paths back and restores the old ones. An incomplete
//! rollback keeps the quarantine and reports its path.
//!
//! Upgrades are not crash-atomic either. The old receipt moves first, so a
//! process death during an upgrade can leave an old keg and links without a
//! receipt, a replacement that is partial or complete, and an `upgrade-*`
//! quarantine below `.brewrs/stages`. There is no journal or automatic
//! recovery: [`Store::installed`] only trusts receipts, a later commit refuses
//! leftover paths, and the remaining files need manual cleanup.
//!
//! # Dependencies
//!
//! A receipt records its direct dependencies as exact `(name, version, sha256)`
//! pins. Under the same writer lock, writers validate the receipt graph they
//! would leave behind before moving any path: every recorded dependency must
//! still be installed at that exact version and digest, so a commit or upgrade
//! can never record a dangling edge, and a removal refuses to strand a
//! dependent that was not selected for the same removal. A batch that removes
//! or upgrades both sides together is coherent. [`validate_dependencies`]
//! exposes that pure check so a preview or fetch can apply the same rules.
//!
//! # Promotion atomicity
//!
//! [`Store::commit_with_promotions`] combines an install with flipping the
//! `requested` flag of exact installed receipts. Each promotion is rechecked
//! under the same writer lock, may only change `false` to `true`, and must still
//! own its real keg. Receipts are replaced atomically in place, and a failed
//! install or promotion removes every new path this transaction created and
//! restores the exact bytes of every receipt it already promoted. Promotions are
//! crash-atomic one receipt at a time; a process death can leave the new
//! installs in place without their later promotions, which is the safe
//! direction.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Ownership marker; its exact content identifies a brewrs prefix.
const MARKER_FILE: &str = ".brewrs-root";
const MARKER_CONTENT: &str = "brewrs native store v1\n";
/// Root that holds installed kegs.
const CELLAR: &str = "Cellar";
/// Root for CLI links.
const BIN: &str = "bin";
/// Directory for brewrs-managed state.
const META: &str = ".brewrs";
const STATE: &str = "state";
const RECEIPTS: &str = "receipts";
const STAGES: &str = "stages";
const LOCK_FILE: &str = "install.lock";
/// Longest accepted package name or version.
const MAX_NAME_LEN: usize = 128;
const MAX_VERSION_LEN: usize = 128;
/// Receipts are tiny; anything larger is corrupt or foreign.
const MAX_RECEIPT_BYTES: u64 = 16 * 1024;

/// Writer lock token/TempDir names stay unique within a process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Where a bound prefix stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootState {
    /// The prefix does not exist (read-only binding only).
    Absent,
    /// The prefix exists but has no brewrs marker; nothing is ours to read.
    Unowned,
    /// The prefix carries a valid marker.
    Owned,
}

/// A brewrs-managed install prefix.
#[derive(Debug)]
pub(super) struct Store {
    prefix: PathBuf,
    state: RootState,
    writable: bool,
}

/// One direct dependency, pinned to an exact name, version, and digest.
///
/// A pin is satisfied only by a receipt with the same name, the same version,
/// and the same bottle digest; a version bump is a different pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Dependency {
    pub name: String,
    pub version: String,
    pub sha256: String,
}

/// The durable record of one installed package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Receipt {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub requested: bool,
    /// Direct dependencies, each pinned to an exact installed name, version,
    /// and digest. Receipts written before dependencies existed omit this
    /// field and decode as empty.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

/// A fully staged package awaiting [`Store::commit`].
pub(super) struct Prepared {
    pub receipt: Receipt,
    /// Root of the unpacked keg; it must come from [`Store::stage`].
    pub stage: tempfile::TempDir,
}

impl Store {
    /// Bind to `prefix`, or initialize it when `create` is true.
    ///
    /// A read-only binding never writes. An absent prefix is represented as an
    /// empty store; an existing directory without the marker is unowned and
    /// also reads as empty.
    pub(super) fn open(prefix: &Path, create: bool) -> Result<Self> {
        if prefix.as_os_str().is_empty() {
            bail!("refusing an empty brewrs prefix");
        }
        if !prefix.is_absolute() {
            bail!(
                "refusing relative brewrs prefix '{}'; use an absolute path",
                prefix.display()
            );
        }
        if create {
            Self::create(prefix)
        } else {
            Self::bind(prefix)
        }
    }

    /// Bind an existing store for a writer without creating or initializing
    /// anything.
    ///
    /// Ownership, the marker, and every managed directory are validated before
    /// the binding is marked writable. An absent, unowned, or foreign prefix is
    /// refused rather than initialized.
    pub(super) fn open_existing(prefix: &Path) -> Result<Self> {
        let mut store = Self::open(prefix, false)?;
        if store.state != RootState::Owned {
            bail!(
                "refusing to modify '{}': it is not an initialized brewrs store",
                prefix.display()
            );
        }
        store.verify_owned()?;
        store.writable = true;
        Ok(store)
    }

    /// The prefix this store is bound to.
    pub(super) fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// Read every receipt, sorted by package name.
    pub(super) fn installed(&self) -> Result<Vec<Receipt>> {
        if self.state != RootState::Owned {
            return Ok(Vec::new());
        }
        if !check_optional_directory(&meta_dir(&self.prefix))?
            || !check_optional_directory(&state_dir(&self.prefix))?
        {
            return Ok(Vec::new());
        }
        let directory = receipts_dir(&self.prefix);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing to read '{}': the receipts directory is a symlink",
                directory.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing to read '{}': the receipts path is not a directory",
                directory.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }

        let mut receipts = Vec::new();
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("failed to read '{}'", directory.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = file_name.strip_suffix(".json") else {
                continue;
            };
            let receipt = read_receipt(&entry.path())?;
            if receipt.name != stem {
                bail!(
                    "receipt '{}' names '{}', not '{}'",
                    entry.path().display(),
                    receipt.name,
                    stem
                );
            }
            receipts.push(receipt);
        }
        receipts.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(receipts)
    }

    /// Create a private staging directory below this store.
    pub(super) fn stage(&self) -> Result<tempfile::TempDir> {
        self.ensure_owned()?;
        let stages = stages_dir(&self.prefix);
        tempfile::Builder::new()
            .prefix("stage-")
            .tempdir_in(&stages)
            .with_context(|| format!("failed to create a stage below '{}'", stages.display()))
    }

    /// Install all prepared packages as one serialized transaction.
    ///
    /// The caller is expected to skip packages that are already installed at
    /// the same version; this method rejects them defensively because it cannot
    /// know whether the caller's view is still current. Under the writer lock
    /// the projected receipt graph is validated first, so every direct
    /// dependency of every receipt this commit would leave behind must exist at
    /// its exact name, version, and digest.
    pub(super) fn commit(&self, packages: Vec<Prepared>) -> Result<()> {
        self.commit_inner(packages, None)
    }

    fn commit_inner(&self, packages: Vec<Prepared>, fail_after: Option<usize>) -> Result<()> {
        self.ensure_owned()?;
        if packages.is_empty() {
            return Ok(());
        }
        // The lock is held for validation as well as application, so receipts
        // and links observed by preflight cannot change underneath us.
        let _lock = InstallLock::acquire(&self.prefix)?;
        let pending = self.prepare(packages)?;
        let mut applied = Vec::new();
        match self.apply(pending, fail_after, &mut applied) {
            Ok(()) => Ok(()),
            Err(error) => {
                let issues = rollback(&applied);
                if issues.is_empty() {
                    Err(error)
                } else {
                    Err(error.context(format!("rollback was incomplete: {}", issues.join("; "))))
                }
            }
        }
    }

    /// Install all prepared packages and mark the exact installed receipts in
    /// `expected` as requested in one serialized transaction.
    ///
    /// This is [`Store::commit`] plus the promotion step an explicit install of
    /// an already installed dependency needs. Each receipt in `expected` must
    /// come from [`Store::installed`] and is re-read under the same writer lock:
    /// a receipt that is missing, changed, or already requested is refused, as
    /// is a duplicate promotion, a promotion whose name also appears in
    /// `packages`, and a package that does not own its real keg. Only the
    /// `requested` flag changes from `false` to `true`.
    ///
    /// New packages are applied exactly as [`Store::commit`] applies them, and
    /// the projected receipt graph (installed packages with the promotions
    /// applied, plus the new packages) is validated before anything is written.
    /// A failed install or promotion rolls back every new path and restores the
    /// exact bytes of every receipt this call already promoted.
    ///
    /// With an empty `expected`, this delegates to [`Store::commit`].
    pub(super) fn commit_with_promotions(
        &self,
        packages: Vec<Prepared>,
        expected: &[Receipt],
    ) -> Result<()> {
        if expected.is_empty() {
            return self.commit(packages);
        }
        self.commit_with_promotions_inner(packages, expected, None, None)
    }

    /// [`Store::commit_with_promotions`] with failure points used by the tests.
    ///
    /// `fail_install_after` fails before that install index, and
    /// `fail_promotion_after` fails before that promotion index after the
    /// preceding promotions were published.
    fn commit_with_promotions_inner(
        &self,
        packages: Vec<Prepared>,
        expected: &[Receipt],
        fail_install_after: Option<usize>,
        fail_promotion_after: Option<usize>,
    ) -> Result<()> {
        self.ensure_owned()?;
        // The lock covers promotion planning, install preflight, application,
        // and rollback, so no concurrent brewrs writer can change a receipt,
        // keg, link, or graph observed by this transaction.
        let _lock = InstallLock::acquire(&self.prefix)?;
        // Ownership may have changed while the lock was being acquired.
        self.ensure_owned()?;

        let pending_names: HashSet<&str> = packages
            .iter()
            .map(|package| package.receipt.name.as_str())
            .collect();
        let promotions = self.plan_promotions(expected, &pending_names)?;
        let pending = self.prepare(packages)?;

        // The complete graph this transaction would leave behind. Promotions
        // change only the requested flag, so they cannot alter a pin, but
        // validating the projected set keeps the mixed transaction honest.
        let mut projected = self.installed()?;
        for receipt in &mut projected {
            if let Some(promotion) = promotions
                .iter()
                .find(|promotion| promotion.promoted.name == receipt.name)
            {
                *receipt = promotion.promoted.clone();
            }
        }
        projected.extend(pending.iter().map(|package| package.receipt.clone()));
        validate_dependencies(&projected)?;

        // New packages are applied first: if they fail, nothing was promoted.
        let mut applied = Vec::new();
        if let Err(error) = self.apply(pending, fail_install_after, &mut applied) {
            return finish_commit(error, rollback(&applied));
        }

        let mut applied_promotions = Vec::new();
        if let Err(error) =
            apply_promotions(promotions, fail_promotion_after, &mut applied_promotions)
        {
            let mut issues = restore_promotions(&applied_promotions);
            issues.extend(rollback(&applied));
            return finish_commit(error, issues);
        }
        Ok(())
    }

    /// Replace the exact installed packages in `packages` as one transaction.
    ///
    /// Each tuple carries the receipt a preview observed and the freshly staged
    /// replacement. Every old receipt, keg, and owned `bin` link is rechecked
    /// under the writer lock before anything moves. The replacement must have
    /// the same name and requested flag, a different version, and no collision
    /// with foreign package or `bin` entries. Old paths are quarantined, new
    /// packages are installed, and a failure rolls the new paths back and
    /// restores the old ones while the caller still holds this process.
    ///
    /// The projected receipt graph is validated first, so an upgrade that would
    /// break a surviving receipt's dependency (a different version or digest)
    /// is rejected unless the dependent is upgraded in the same call.
    ///
    /// Like commits and removals, upgrades are not crash-atomic; see the module
    /// documentation for the files an interrupted upgrade can leave behind.
    pub(super) fn upgrade(&self, packages: Vec<(Receipt, Prepared)>) -> Result<()> {
        self.upgrade_inner(packages, None)
    }

    /// [`Store::upgrade`] with a failure point used by the tests.
    fn upgrade_inner(
        &self,
        packages: Vec<(Receipt, Prepared)>,
        fail_after: Option<usize>,
    ) -> Result<()> {
        self.ensure_owned()?;
        if packages.is_empty() {
            return Ok(());
        }
        // The lock covers validation, quarantine, install, and rollback, so no
        // concurrent brewrs writer can change receipts, links, or kegs.
        let _lock = InstallLock::acquire(&self.prefix)?;
        // Ownership may have changed while the lock was being acquired.
        self.ensure_owned()?;
        let upgrades = self.plan_upgrade(packages)?;
        self.apply_upgrade(upgrades, fail_after)
    }

    /// Remove exactly the packages named by `expected`.
    ///
    /// `expected` must come from a previous [`Store::installed`] call. Every
    /// receipt is rechecked under the same writer lock [`Store::commit`] uses,
    /// so a stale selection (a different version, digest, or requested flag, or
    /// a receipt that is already gone) is rejected before any path moves. The
    /// caller may hold a read-only binding: removal never initializes a prefix.
    ///
    /// The projected receipt graph is validated as well: a receipt that remains
    /// installed and depends on a package this call removes blocks the removal
    /// with an error naming both packages. Selecting the dependent in the same
    /// call allows the batch.
    ///
    /// It removes the receipt, the receipt-owned keg, and only the `bin` links
    /// whose stored target is exactly the relative link [`Store::commit`]
    /// created for that package and version, including a link left dangling by
    /// a deleted keg member or stale by a non-executable one. Foreign files,
    /// directories, and links are preserved and never followed.
    pub(super) fn remove(&self, expected: &[Receipt]) -> Result<()> {
        self.remove_inner(expected, None)
    }

    /// [`Store::remove`] with a failure point used by the tests.
    fn remove_inner(&self, expected: &[Receipt], fail_after: Option<usize>) -> Result<()> {
        self.verify_owned()?;
        if expected.is_empty() {
            return Ok(());
        }
        // The lock covers the recheck and every move, so receipts, links, and
        // kegs cannot change underneath a concurrent brewrs writer.
        let _lock = InstallLock::acquire(&self.prefix)?;
        // Ownership may have changed while the lock was being acquired.
        self.verify_owned()?;
        let removals = self.plan_removal(expected)?;
        self.apply_removal(removals, fail_after)
    }

    /// Check every selected receipt, keg, and CLI link before moving anything.
    fn plan_removal(&self, expected: &[Receipt]) -> Result<Vec<Removal>> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let bin = bin_dir(&self.prefix);
        let mut names = HashSet::new();
        let mut links_seen = HashSet::new();
        let mut removals = Vec::with_capacity(expected.len());

        for receipt in expected {
            validate_receipt(receipt)?;
            if !names.insert(receipt.name.clone()) {
                bail!(
                    "package '{}' appears more than once in one removal",
                    receipt.name
                );
            }

            let receipt_path = receipts.join(format!("{}.json", receipt.name));
            let current = existing_receipt(&receipt_path)?.with_context(|| {
                format!(
                    "refusing to remove '{} {}': its receipt '{}' no longer exists",
                    receipt.name,
                    receipt.version,
                    receipt_path.display()
                )
            })?;
            if current != *receipt {
                bail!(
                    "refusing to remove '{} {}': the installed receipt now records {} {}; re-read the installed packages and try again",
                    receipt.name,
                    receipt.version,
                    current.name,
                    current.version
                );
            }

            let package = cellar.join(&receipt.name);
            match fs::symlink_metadata(&package) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to remove '{}': '{}' is a symlink",
                    receipt.name,
                    package.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to remove '{}': '{}' is not a directory",
                    receipt.name,
                    package.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to remove '{} {}': '{}' is missing; remove it manually",
                    receipt.name,
                    receipt.version,
                    package.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", package.display()));
                }
            }

            let keg = package.join(&receipt.version);
            match fs::symlink_metadata(&keg) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to remove '{} {}': '{}' is a symlink",
                    receipt.name,
                    receipt.version,
                    keg.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to remove '{} {}': '{}' is not a directory",
                    receipt.name,
                    receipt.version,
                    keg.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to remove '{} {}': its keg '{}' is missing; remove it manually",
                    receipt.name,
                    receipt.version,
                    keg.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", keg.display()));
                }
            }

            // Scan the prefix's own bin entries. Deriving names from the keg
            // would miss an owned link whose keg member was deleted or made
            // non-executable, and a reinstall would then collide with it. Only
            // an exact stored target identifies a brewrs link; everything else
            // is foreign and is preserved.
            let mut links = Vec::new();
            for entry in
                fs::read_dir(&bin).with_context(|| format!("failed to read '{}'", bin.display()))?
            {
                let entry = entry
                    .with_context(|| format!("failed to read an entry in '{}'", bin.display()))?;
                if !entry
                    .file_type()
                    .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?
                    .is_symlink()
                {
                    continue;
                }
                let Some(member) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !is_safe_bin_name(&member) {
                    continue;
                }
                let path = entry.path();
                let target = keg_link_target(&receipt.name, &receipt.version, &member);
                let actual = fs::read_link(&path)
                    .with_context(|| format!("failed to read link '{}'", path.display()))?;
                if actual.as_os_str() != target.as_os_str() {
                    continue;
                }
                if !links_seen.insert(path.clone()) {
                    bail!(
                        "refusing to remove '{}': more than one selected package claims the link '{}'",
                        receipt.name,
                        path.display()
                    );
                }
                links.push(OwnedLink { path, target });
            }
            links.sort_by(|left, right| left.path.cmp(&right.path));

            removals.push(Removal {
                receipt_path,
                package,
                keg,
                links,
            });
        }

        // A removal must not strand a dependent that stays installed. The graph
        // this call would leave behind is the installed set minus the selection;
        // `installed` runs under the same lock as the moves below, so the check
        // sees exactly the receipts those moves will delete.
        let selected: HashSet<&str> = expected
            .iter()
            .map(|receipt| receipt.name.as_str())
            .collect();
        let remaining: Vec<Receipt> = self
            .installed()?
            .into_iter()
            .filter(|receipt| !selected.contains(receipt.name.as_str()))
            .collect();
        validate_dependencies(&remaining)?;
        Ok(removals)
    }

    /// Move preflighted paths into a private quarantine, rolling back on error.
    fn apply_removal(&self, removals: Vec<Removal>, fail_after: Option<usize>) -> Result<()> {
        let stages = stages_dir(&self.prefix);
        let quarantine = tempfile::Builder::new()
            .prefix("remove-")
            .tempdir_in(&stages)
            .with_context(|| {
                format!(
                    "failed to create a removal quarantine below '{}'",
                    stages.display()
                )
            })?;

        let mut steps = Vec::new();
        for removal in removals {
            steps.push(RemovalStep {
                source: removal.receipt_path,
                kind: StepKind::Receipt,
            });
            for link in removal.links {
                steps.push(RemovalStep {
                    source: link.path,
                    kind: StepKind::Link {
                        target: link.target,
                    },
                });
            }
            steps.push(RemovalStep {
                source: removal.keg,
                kind: StepKind::Directory,
            });
            // Move only the exact version directory: the package directory may
            // gain foreign entries between preflight and this rename. Once the
            // keg is parked, an empty package directory is pruned with a
            // non-recursive removal that can never delete a foreign entry.
            steps.push(RemovalStep {
                source: removal.package,
                kind: StepKind::Prune,
            });
        }

        let mut applied = Vec::new();
        let outcome = (|| -> Result<()> {
            for (index, step) in steps.iter().enumerate() {
                if fail_after == Some(index) {
                    bail!("injected removal failure before move {index}");
                }
                if matches!(&step.kind, StepKind::Prune) {
                    prune_package_dir(step, &mut applied)?;
                } else {
                    move_aside(step, quarantine.path(), index, &mut applied)?;
                }
            }
            Ok(())
        })();

        match outcome {
            Ok(()) => {
                let mut flushed = HashSet::new();
                for change in &applied {
                    let source = match change {
                        AppliedChange::Moved(moved) => &moved.source,
                        AppliedChange::Pruned(directory) => directory,
                    };
                    if let Some(parent) = source.parent() {
                        if flushed.insert(parent.to_path_buf()) {
                            sync_directory(parent);
                        }
                    }
                }
                if let Err(error) = quarantine.close() {
                    log::warn!("could not delete the removal quarantine: {error}");
                }
                Ok(())
            }
            Err(error) => {
                let issues = rollback_removal(&applied);
                if issues.is_empty() {
                    drop(quarantine);
                    Err(error)
                } else {
                    // Keep the quarantine: dropping it would delete paths that
                    // are still needed for a manual rollback.
                    let kept = quarantine.keep();
                    Err(error.context(format!(
                        "rollback was incomplete: {}; the quarantined paths were kept in '{}'",
                        issues.join("; "),
                        kept.display()
                    )))
                }
            }
        }
    }

    /// Check every old receipt, keg, and owned link and every replacement
    /// before moving anything.
    fn plan_upgrade(&self, packages: Vec<(Receipt, Prepared)>) -> Result<Vec<Upgrade>> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let bin = bin_dir(&self.prefix);
        let mut names = HashSet::new();
        let mut link_names = HashSet::new();
        let mut upgrades = Vec::with_capacity(packages.len());

        for (old, prepared) in packages {
            let Prepared { receipt, stage } = prepared;
            validate_receipt(&old)?;
            validate_receipt(&receipt)?;
            if old.name != receipt.name {
                bail!(
                    "refusing to upgrade '{}': the staged package is '{}'",
                    old.name,
                    receipt.name
                );
            }
            if !names.insert(old.name.clone()) {
                bail!(
                    "package '{}' appears more than once in one upgrade",
                    old.name
                );
            }
            if old.requested != receipt.requested {
                bail!(
                    "refusing to upgrade '{} {}': the replacement would change its requested status",
                    old.name,
                    old.version
                );
            }
            if old.version == receipt.version {
                bail!(
                    "refusing to upgrade '{}': version {} is already installed; the caller must skip it",
                    old.name,
                    old.version
                );
            }
            self.validate_stage(stage.path())?;
            let links = executable_bin_members(stage.path())?;

            let receipt_path = receipts.join(format!("{}.json", old.name));
            let current = existing_receipt(&receipt_path)?.with_context(|| {
                format!(
                    "refusing to upgrade '{} {}': its receipt '{}' no longer exists",
                    old.name,
                    old.version,
                    receipt_path.display()
                )
            })?;
            if current != old {
                bail!(
                    "refusing to upgrade '{} {}': the installed receipt now records {} {}; re-read the installed packages and try again",
                    old.name,
                    old.version,
                    current.name,
                    current.version
                );
            }

            let package = cellar.join(&old.name);
            match fs::symlink_metadata(&package) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to upgrade '{}': '{}' is a symlink",
                    old.name,
                    package.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to upgrade '{}': '{}' is not a directory",
                    old.name,
                    package.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to upgrade '{} {}': '{}' is missing; remove it manually",
                    old.name,
                    old.version,
                    package.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", package.display()));
                }
            }

            let keg = package.join(&old.version);
            match fs::symlink_metadata(&keg) {
                Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                    "refusing to upgrade '{} {}': '{}' is a symlink",
                    old.name,
                    old.version,
                    keg.display()
                ),
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to upgrade '{} {}': '{}' is not a directory",
                    old.name,
                    old.version,
                    keg.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "refusing to upgrade '{} {}': its keg '{}' is missing; remove it manually",
                    old.name,
                    old.version,
                    keg.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", keg.display()));
                }
            }

            // The package directory must hold exactly the receipt's keg. An
            // alternate version or any other entry would survive the old keg
            // moving aside and collide with the replacement.
            for entry in fs::read_dir(&package)
                .with_context(|| format!("failed to read '{}'", package.display()))?
            {
                let entry = entry.with_context(|| {
                    format!("failed to read an entry in '{}'", package.display())
                })?;
                if entry.file_name().as_os_str() != OsStr::new(&old.version) {
                    bail!(
                        "refusing to upgrade '{}': '{}' contains '{}', which is not the installed version {}; remove foreign entries first",
                        old.name,
                        package.display(),
                        entry.file_name().to_string_lossy(),
                        old.version
                    );
                }
            }

            let mut owned_links = Vec::new();
            for entry in
                fs::read_dir(&bin).with_context(|| format!("failed to read '{}'", bin.display()))?
            {
                let entry = entry
                    .with_context(|| format!("failed to read an entry in '{}'", bin.display()))?;
                if !entry
                    .file_type()
                    .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?
                    .is_symlink()
                {
                    continue;
                }
                let Some(member) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !is_safe_bin_name(&member) {
                    continue;
                }
                let path = entry.path();
                let target = keg_link_target(&old.name, &old.version, &member);
                let actual = fs::read_link(&path)
                    .with_context(|| format!("failed to read link '{}'", path.display()))?;
                if actual.as_os_str() == target.as_os_str() {
                    owned_links.push(OwnedLink { path, target });
                }
            }
            owned_links.sort_by(|left, right| left.path.cmp(&right.path));

            // A replacement may claim a bin name only when this package's own
            // exact link currently occupies it; anything else is foreign and
            // is never overwritten. Cross-package claims fail in `link_names`.
            for link in &links {
                if !link_names.insert(link.clone()) {
                    bail!("more than one package in this upgrade provides 'bin/{link}'");
                }
                let existing = bin.join(link);
                match fs::symlink_metadata(&existing) {
                    Ok(_) => {
                        if !owned_links.iter().any(|owned| owned.path == existing) {
                            bail!(
                                "refusing to upgrade '{}': '{}' already exists and is not a link owned by {} {}; refusing to overwrite it",
                                old.name,
                                existing.display(),
                                old.name,
                                old.version
                            );
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", existing.display())
                        });
                    }
                }
            }

            upgrades.push(Upgrade {
                old: Removal {
                    receipt_path,
                    package,
                    keg,
                    links: owned_links,
                },
                pending: Pending {
                    receipt,
                    stage,
                    links,
                },
            });
        }

        // Validate the graph this upgrade would leave behind: every surviving
        // receipt plus every replacement. Without the replacement in the same
        // batch, a surviving package still pinned to the old version or digest
        // blocks the upgrade, so a dependency bump cannot silently strand it.
        let mut final_receipts: Vec<Receipt> = self
            .installed()?
            .into_iter()
            .filter(|receipt| !names.contains(&receipt.name))
            .collect();
        final_receipts.extend(
            upgrades
                .iter()
                .map(|upgrade| upgrade.pending.receipt.clone()),
        );
        validate_dependencies(&final_receipts)?;
        Ok(upgrades)
    }

    /// Quarantine the preflighted old paths, apply the replacements, and undo
    /// both sides if the install fails.
    fn apply_upgrade(&self, upgrades: Vec<Upgrade>, fail_after: Option<usize>) -> Result<()> {
        let stages = stages_dir(&self.prefix);
        let quarantine = tempfile::Builder::new()
            .prefix("upgrade-")
            .tempdir_in(&stages)
            .with_context(|| {
                format!(
                    "failed to create an upgrade quarantine below '{}'",
                    stages.display()
                )
            })?;

        let mut moved = Vec::new();
        let move_outcome = (|| -> Result<()> {
            let mut index = 0usize;
            for upgrade in &upgrades {
                let steps = std::iter::once(RemovalStep {
                    source: upgrade.old.receipt_path.clone(),
                    kind: StepKind::Receipt,
                })
                .chain(upgrade.old.links.iter().map(|link| RemovalStep {
                    source: link.path.clone(),
                    kind: StepKind::Link {
                        target: link.target.clone(),
                    },
                }))
                .chain(std::iter::once(RemovalStep {
                    source: upgrade.old.keg.clone(),
                    kind: StepKind::Directory,
                }));
                for step in steps {
                    move_aside(&step, quarantine.path(), index, &mut moved)?;
                    index += 1;
                }
            }
            Ok(())
        })();

        if let Err(error) = move_outcome {
            return finish_upgrade(quarantine, &[], &moved, error);
        }

        let pending: Vec<_> = upgrades
            .into_iter()
            .map(|upgrade| upgrade.pending)
            .collect();
        let mut applied = Vec::new();
        match self.apply_inner(pending, fail_after, &mut applied, true) {
            Ok(()) => {
                let mut flushed = HashSet::new();
                for change in &moved {
                    let AppliedChange::Moved(moved) = change else {
                        continue;
                    };
                    if let Some(parent) = moved.source.parent() {
                        if flushed.insert(parent.to_path_buf()) {
                            sync_directory(parent);
                        }
                    }
                }
                if let Err(error) = quarantine.close() {
                    log::warn!("could not delete the upgrade quarantine: {error}");
                }
                Ok(())
            }
            Err(error) => finish_upgrade(quarantine, &applied, &moved, error),
        }
    }

    /// Bind without writing.
    fn bind(prefix: &Path) -> Result<Self> {
        match fs::symlink_metadata(prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing '{}': a brewrs prefix may not be a symlink",
                prefix.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing '{}': a brewrs prefix must be a directory",
                prefix.display()
            ),
            Ok(_) => {
                let state = match marker_state(prefix)? {
                    MarkerState::Valid => RootState::Owned,
                    MarkerState::Missing => RootState::Unowned,
                };
                Ok(Self {
                    prefix: prefix.to_path_buf(),
                    state,
                    writable: false,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self {
                prefix: prefix.to_path_buf(),
                state: RootState::Absent,
                writable: false,
            }),
            Err(error) => Err(error)
                .with_context(|| format!("failed to inspect prefix '{}'", prefix.display())),
        }
    }

    /// Initialize or reuse a dedicated prefix.
    fn create(prefix: &Path) -> Result<Self> {
        reject_protected_prefix(prefix)?;
        match fs::symlink_metadata(prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing '{}': a brewrs prefix may not be a symlink",
                prefix.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing '{}': a brewrs prefix must be a directory",
                prefix.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(prefix)
                    .with_context(|| format!("failed to create prefix '{}'", prefix.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect prefix '{}'", prefix.display()));
            }
        }

        // Re-check after creation: a path component may have appeared as a
        // symlink while the directories were being made.
        reject_protected_prefix(prefix)?;
        let metadata = fs::symlink_metadata(prefix)
            .with_context(|| format!("failed to inspect prefix '{}'", prefix.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing '{}': the prefix is not a real directory",
                prefix.display()
            );
        }

        match marker_state(prefix)? {
            MarkerState::Valid => {}
            MarkerState::Missing => {
                if fs::read_dir(prefix)
                    .with_context(|| format!("failed to read prefix '{}'", prefix.display()))?
                    .next()
                    .is_some()
                {
                    // Another initializer may have won the marker race after
                    // our first check; that is not a foreign directory.
                    if marker_state(prefix)? == MarkerState::Missing {
                        bail!(
                            "refusing to use '{}': it is not empty and has no {} marker; choose an empty directory or an existing brewrs prefix",
                            prefix.display(),
                            MARKER_FILE
                        );
                    }
                } else {
                    create_marker(prefix)?;
                }
            }
        }

        for directory in [
            cellar_dir(prefix),
            bin_dir(prefix),
            meta_dir(prefix),
            state_dir(prefix),
            receipts_dir(prefix),
            stages_dir(prefix),
        ] {
            ensure_directory(&directory, true)?;
        }

        Ok(Self {
            prefix: prefix.to_path_buf(),
            state: RootState::Owned,
            writable: true,
        })
    }

    /// Re-validate ownership and every managed directory before writing.
    fn ensure_owned(&self) -> Result<()> {
        if !self.writable {
            bail!(
                "refusing to modify '{}': the store was opened read-only",
                self.prefix.display()
            );
        }
        self.verify_owned()
    }

    /// Re-validate ownership without requiring a writable binding.
    ///
    /// Removal runs on [`Store::open`]'s read-only binding, so it uses this to
    /// refuse an absent, unowned, or foreign prefix before writing anything.
    fn verify_owned(&self) -> Result<()> {
        if self.state != RootState::Owned {
            bail!(
                "refusing to modify '{}': it is not an initialized brewrs store",
                self.prefix.display()
            );
        }
        // Reject a protected location before looking at its contents: a
        // planted marker must not make a Homebrew prefix look manageable.
        reject_protected_prefix(&self.prefix)?;
        let metadata = fs::symlink_metadata(&self.prefix)
            .with_context(|| format!("failed to inspect prefix '{}'", self.prefix.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing '{}': the prefix is no longer a real directory",
                self.prefix.display()
            );
        }
        match marker_state(&self.prefix)? {
            MarkerState::Valid => {}
            MarkerState::Missing => bail!(
                "refusing '{}': the {} marker disappeared",
                self.prefix.display(),
                MARKER_FILE
            ),
        }
        for directory in [
            cellar_dir(&self.prefix),
            bin_dir(&self.prefix),
            meta_dir(&self.prefix),
            state_dir(&self.prefix),
            receipts_dir(&self.prefix),
            stages_dir(&self.prefix),
        ] {
            ensure_directory(&directory, false)?;
        }
        Ok(())
    }

    /// Check the whole batch against existing state before writing anything.
    fn prepare(&self, packages: Vec<Prepared>) -> Result<Vec<Pending>> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let bin = bin_dir(&self.prefix);
        let mut names = HashSet::new();
        let mut link_names = HashSet::new();
        let mut pending = Vec::with_capacity(packages.len());

        for Prepared { receipt, stage } in packages {
            validate_receipt(&receipt)?;
            if !names.insert(receipt.name.clone()) {
                bail!(
                    "package '{}' appears more than once in one commit",
                    receipt.name
                );
            }
            self.validate_stage(stage.path())?;
            let links = executable_bin_members(stage.path())?;

            let receipt_path = receipts.join(format!("{}.json", receipt.name));
            if let Some(existing) = existing_receipt(&receipt_path)? {
                if existing.version == receipt.version {
                    bail!(
                        "'{} {}' is already installed in '{}'; the caller must skip it",
                        receipt.name,
                        receipt.version,
                        self.prefix.display()
                    );
                }
                bail!(
                    "'{}' is already installed at version {}; refusing to replace it with {}",
                    receipt.name,
                    existing.version,
                    receipt.version
                );
            }

            let installed_dir = cellar.join(&receipt.name);
            match fs::symlink_metadata(&installed_dir) {
                Ok(_) => bail!(
                    "refusing to install '{}': '{}' already exists without a receipt; remove it or use a clean prefix",
                    receipt.name,
                    installed_dir.display()
                ),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect '{}'", installed_dir.display())
                    });
                }
            }

            for link in &links {
                if !link_names.insert(link.clone()) {
                    bail!("more than one package in this commit provides 'bin/{link}'");
                }
                let existing = bin.join(link);
                match fs::symlink_metadata(&existing) {
                    Ok(_) => bail!(
                        "refusing to install '{}': '{}' already exists; links are never overwritten",
                        receipt.name,
                        existing.display()
                    ),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", existing.display())
                        });
                    }
                }
            }

            pending.push(Pending {
                receipt,
                stage,
                links,
            });
        }

        // Full preflight of the graph this commit would leave behind: the
        // receipts already on disk plus the ones about to be written. A
        // dependency may name a package from this same batch, but its exact
        // version and digest must match.
        let mut final_receipts = self.installed()?;
        final_receipts.extend(pending.iter().map(|package| package.receipt.clone()));
        validate_dependencies(&final_receipts)?;
        Ok(pending)
    }

    /// Recheck every promotion against the current receipts and kegs.
    ///
    /// Runs under the writer lock. A promotion may only flip `requested` from
    /// false to true on the exact receipt the caller observed, so any stale or
    /// changed receipt is refused before a single path is written.
    fn plan_promotions(
        &self,
        expected: &[Receipt],
        pending_names: &HashSet<&str>,
    ) -> Result<Vec<Promotion>> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let mut names = HashSet::new();
        let mut promotions = Vec::with_capacity(expected.len());

        for receipt in expected {
            validate_receipt(receipt)?;
            if !names.insert(receipt.name.clone()) {
                bail!(
                    "package '{}' appears more than once in one promotion",
                    receipt.name
                );
            }
            if pending_names.contains(receipt.name.as_str()) {
                bail!(
                    "refusing to install and promote '{}' in the same call",
                    receipt.name
                );
            }
            if receipt.requested {
                bail!(
                    "refusing to promote '{} {}': the observed receipt is already requested",
                    receipt.name,
                    receipt.version
                );
            }

            let path = receipts.join(format!("{}.json", receipt.name));
            let (current, original) = read_receipt_with_bytes(&path).with_context(|| {
                format!(
                    "refusing to promote '{} {}': its receipt '{}' could not be re-read",
                    receipt.name,
                    receipt.version,
                    path.display()
                )
            })?;
            if current != *receipt {
                bail!(
                    "refusing to promote '{} {}': the installed receipt now records {} {} with requested={}; re-read the installed packages and try again",
                    receipt.name,
                    receipt.version,
                    current.name,
                    current.version,
                    current.requested
                );
            }
            validate_promotion_keg(&receipt.name, &receipt.version, &cellar)?;

            let mut promoted = receipt.clone();
            promoted.requested = true;
            let bytes = encode_receipt(&promoted)?;
            promotions.push(Promotion {
                promoted,
                path,
                original,
                bytes,
            });
        }
        Ok(promotions)
    }

    /// A stage must be a real directory directly inside this store's stages.
    fn validate_stage(&self, stage: &Path) -> Result<()> {
        let stages = stages_dir(&self.prefix);
        let expected = stages
            .canonicalize()
            .with_context(|| format!("failed to resolve '{}'", stages.display()))?;
        let metadata = fs::symlink_metadata(stage)
            .with_context(|| format!("failed to inspect stage '{}'", stage.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing stage '{}': it is not a real directory",
                stage.display()
            );
        }
        let actual = stage
            .canonicalize()
            .with_context(|| format!("failed to resolve stage '{}'", stage.display()))?;
        if actual.parent() != Some(expected.as_path()) {
            bail!(
                "refusing stage '{}': it is not owned by '{}'",
                stage.display(),
                stages.display()
            );
        }
        Ok(())
    }

    /// Apply validated packages, recording each owned path for rollback.
    fn apply(
        &self,
        pending: Vec<Pending>,
        fail_after: Option<usize>,
        applied: &mut Vec<Applied>,
    ) -> Result<()> {
        self.apply_inner(pending, fail_after, applied, false)
    }

    /// Apply validated packages with an optional pre-existing package
    /// directory, which an upgrade leaves in place after moving its old keg.
    fn apply_inner(
        &self,
        pending: Vec<Pending>,
        fail_after: Option<usize>,
        applied: &mut Vec<Applied>,
        reuse_cellar: bool,
    ) -> Result<()> {
        let cellar = cellar_dir(&self.prefix);
        let bin = bin_dir(&self.prefix);
        let receipts = receipts_dir(&self.prefix);

        for (index, package) in pending.into_iter().enumerate() {
            if fail_after == Some(index) {
                bail!("injected commit failure before package {index}");
            }
            let Pending {
                receipt,
                stage,
                links,
            } = package;
            let name = receipt.name.clone();
            let version = receipt.version.clone();

            let package_cellar = cellar.join(&name);
            if !reuse_cellar {
                fs::create_dir(&package_cellar).with_context(|| {
                    format!(
                        "failed to create '{}' for {} {}",
                        package_cellar.display(),
                        name,
                        version
                    )
                })?;
            }
            let keg = package_cellar.join(&version);
            applied.push(Applied {
                cellar: package_cellar.clone(),
                cellar_created: !reuse_cellar,
                keg: keg.clone(),
                keg_created: false,
                links: Vec::new(),
                receipt: receipts.join(format!("{name}.json")),
            });

            fs::rename(stage.path(), &keg).with_context(|| {
                format!(
                    "failed to move the staged {} {} into '{}'",
                    name,
                    version,
                    keg.display()
                )
            })?;

            let record = applied.last_mut().expect("record was just pushed");
            record.keg_created = true;
            for link in links {
                let link_path = bin.join(&link);
                let target = keg_link_target(&name, &version, &link);
                create_symlink(&target, &link_path).with_context(|| {
                    format!("failed to create the CLI link '{}'", link_path.display())
                })?;
                record.links.push(link_path);
            }

            write_receipt(&receipts, &receipt)?;
        }
        Ok(())
    }
}

/// A validated package awaiting application.
struct Pending {
    receipt: Receipt,
    stage: tempfile::TempDir,
    links: Vec<String>,
}

/// A preflighted requested-flag update for one installed package.
struct Promotion {
    /// The receipt this promotion would publish, with `requested` set to true.
    promoted: Receipt,
    path: PathBuf,
    /// Exact receipt bytes before the promotion, restored by rollback.
    original: Vec<u8>,
    /// Exact receipt bytes this promotion publishes.
    bytes: Vec<u8>,
}

/// A validated replacement with the old package it detaches first.
struct Upgrade {
    old: Removal,
    pending: Pending,
}

/// Paths owned and created by one applied package.
struct Applied {
    cellar: PathBuf,
    /// False when the package directory already existed for an upgrade.
    cellar_created: bool,
    /// The exact keg this apply created, removed when the cellar is reused.
    keg: PathBuf,
    /// Whether the staged keg was successfully renamed into place.
    keg_created: bool,
    links: Vec<PathBuf>,
    receipt: PathBuf,
}

/// A preflighted package removal.
struct Removal {
    receipt_path: PathBuf,
    /// `Cellar/<name>`; pruned only once it is empty.
    package: PathBuf,
    keg: PathBuf,
    links: Vec<OwnedLink>,
}

/// A `bin` link whose stored target exactly matches the one `commit` wrote.
struct OwnedLink {
    path: PathBuf,
    target: PathBuf,
}

/// A preflighted path to move aside, in application order.
struct RemovalStep {
    source: PathBuf,
    kind: StepKind,
}

enum StepKind {
    /// Must still be the regular receipt read during preflight.
    Receipt,
    /// Must still be a symlink with exactly this stored target.
    Link { target: PathBuf },
    /// Must still be a real directory: the receipt-owned keg.
    Directory,
    /// The emptied package directory to remove without recursion.
    Prune,
}

/// A reversible change made by one removal.
enum AppliedChange {
    /// A path moved into the removal quarantine.
    Moved(MovedPath),
    /// An empty package directory that was removed.
    Pruned(PathBuf),
}

/// A path moved into the removal quarantine.
struct MovedPath {
    source: PathBuf,
    parked: PathBuf,
}

/// Remove every path created by a failed commit, newest package first.
///
/// Only paths this commit created are removed. A receipt that cannot be
/// removed is reported and its package is left untouched rather than exposing
/// a receipt whose files are gone.
fn rollback(applied: &[Applied]) -> Vec<String> {
    let mut issues = Vec::new();
    for record in applied.iter().rev() {
        let receipt_removed = match fs::symlink_metadata(&record.receipt) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                issues.push(format!(
                    "left '{}': it is not a regular receipt",
                    record.receipt.display()
                ));
                false
            }
            Ok(_) => match fs::remove_file(&record.receipt) {
                Ok(()) => true,
                Err(error) => {
                    issues.push(format!(
                        "failed to remove '{}': {error}",
                        record.receipt.display()
                    ));
                    false
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => {
                issues.push(format!(
                    "failed to inspect '{}': {error}",
                    record.receipt.display()
                ));
                false
            }
        };
        if !receipt_removed {
            continue;
        }

        for link in record.links.iter().rev() {
            match fs::symlink_metadata(link) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if let Err(error) = fs::remove_file(link) {
                        issues.push(format!("failed to remove '{}': {error}", link.display()));
                    }
                }
                Ok(_) => issues.push(format!(
                    "left '{}': it is no longer a brewrs symlink",
                    link.display()
                )),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    issues.push(format!("failed to inspect '{}': {error}", link.display()));
                }
            }
        }

        let removed = if record.cellar_created {
            Some((&record.cellar, fs::remove_dir_all(&record.cellar)))
        } else if record.keg_created {
            // The package directory belongs to the old install and is needed
            // for restoring its keg; only the new keg may be removed.
            Some((&record.keg, fs::remove_dir_all(&record.keg)))
        } else {
            None
        };
        if let Some((path, removed)) = removed {
            match removed {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    issues.push(format!("failed to remove '{}': {error}", path.display()))
                }
            }
        }
    }
    issues
}

/// Report a failed transaction, annotating it when rollback was incomplete.
fn finish_commit(error: anyhow::Error, issues: Vec<String>) -> Result<()> {
    if issues.is_empty() {
        Err(error)
    } else {
        Err(error.context(format!("rollback was incomplete: {}", issues.join("; "))))
    }
}

/// Undo a failed upgrade: remove the new paths, then restore the old ones.
///
/// The quarantine is kept when either side could not be fully undone, because
/// dropping it would delete paths still needed for manual recovery.
fn finish_upgrade(
    quarantine: tempfile::TempDir,
    applied: &[Applied],
    moved: &[AppliedChange],
    error: anyhow::Error,
) -> Result<()> {
    let mut issues = rollback(applied);
    issues.extend(rollback_removal(moved));
    if issues.is_empty() {
        drop(quarantine);
        Err(error)
    } else {
        let kept = quarantine.keep();
        Err(error.context(format!(
            "rollback was incomplete: {}; the quarantined old paths were kept in '{}'",
            issues.join("; "),
            kept.display()
        )))
    }
}

/// Move one preflighted path into the quarantine, rechecking its exact type.
///
/// Renaming a symlink moves the link itself, so this never follows a foreign
/// target. A path that changed since preflight aborts the whole removal.
fn move_aside(
    step: &RemovalStep,
    quarantine: &Path,
    index: usize,
    applied: &mut Vec<AppliedChange>,
) -> Result<()> {
    let source = &step.source;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to remove '{}': it disappeared after preflight",
            source.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", source.display()));
        }
    };
    match &step.kind {
        StepKind::Receipt if metadata.file_type().is_symlink() || !metadata.is_file() => bail!(
            "refusing to remove '{}': it is no longer a regular receipt",
            source.display()
        ),
        StepKind::Directory if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "refusing to remove '{}': it is no longer a real directory",
            source.display()
        ),
        StepKind::Link { target } => {
            if !metadata.file_type().is_symlink() {
                bail!(
                    "refusing to remove '{}': it is no longer a brewrs symlink",
                    source.display()
                );
            }
            let actual = fs::read_link(source)
                .with_context(|| format!("failed to read link '{}'", source.display()))?;
            if actual.as_os_str() != target.as_os_str() {
                bail!(
                    "refusing to remove '{}': the link target is now '{}'",
                    source.display(),
                    actual.display()
                );
            }
        }
        _ => {}
    }

    let parked = quarantine.join(format!("item-{index}"));
    fs::rename(source, &parked).with_context(|| {
        format!(
            "failed to move '{}' into the removal quarantine",
            source.display()
        )
    })?;
    applied.push(AppliedChange::Moved(MovedPath {
        source: source.clone(),
        parked,
    }));
    Ok(())
}

/// Remove the package directory, but only while it is empty.
///
/// A non-recursive removal cannot delete a foreign entry that appeared after
/// preflight: it fails with `DirectoryNotEmpty` and the directory is kept.
fn prune_package_dir(step: &RemovalStep, applied: &mut Vec<AppliedChange>) -> Result<()> {
    let directory = &step.source;
    match fs::remove_dir(directory) {
        Ok(()) => {
            applied.push(AppliedChange::Pruned(directory.clone()));
            Ok(())
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove the emptied package directory '{}'",
                directory.display()
            )
        }),
    }
}

/// Undo every applied removal, newest change first.
fn rollback_removal(applied: &[AppliedChange]) -> Vec<String> {
    let mut issues = Vec::new();
    for change in applied.iter().rev() {
        match change {
            AppliedChange::Pruned(directory) => match fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => issues.push(format!(
                    "failed to recreate '{}': {error}",
                    directory.display()
                )),
            },
            AppliedChange::Moved(moved) => {
                match fs::symlink_metadata(&moved.source) {
                    Ok(_) => {
                        issues.push(format!(
                            "left '{}' in quarantine: the path reappeared",
                            moved.source.display()
                        ));
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        issues.push(format!(
                            "failed to inspect '{}': {error}",
                            moved.source.display()
                        ));
                        continue;
                    }
                }
                if let Err(error) = fs::rename(&moved.parked, &moved.source) {
                    issues.push(format!(
                        "failed to restore '{}': {error}",
                        moved.source.display()
                    ));
                }
            }
        }
    }
    issues
}

/// Publish every preflighted promotion, recording each for rollback.
fn apply_promotions(
    promotions: Vec<Promotion>,
    fail_after: Option<usize>,
    applied: &mut Vec<Promotion>,
) -> Result<()> {
    for (index, promotion) in promotions.into_iter().enumerate() {
        if fail_after == Some(index) {
            bail!("injected promotion failure before promotion {index}");
        }
        let current = fs::read(&promotion.path).with_context(|| {
            format!(
                "failed to re-read '{}' before promoting '{} {}'",
                promotion.path.display(),
                promotion.promoted.name,
                promotion.promoted.version
            )
        })?;
        if current != promotion.original {
            bail!(
                "refusing to promote '{} {}': its receipt changed after preflight",
                promotion.promoted.name,
                promotion.promoted.version
            );
        }
        replace_receipt(&promotion.path, &promotion.bytes).with_context(|| {
            format!(
                "failed to mark '{} {}' as requested",
                promotion.promoted.name, promotion.promoted.version
            )
        })?;
        applied.push(promotion);
    }
    Ok(())
}

/// Restore the exact receipt bytes of every promotion this transaction
/// published, newest first.
///
/// A receipt that changed after its promotion is left alone and reported, so a
/// rollback never overwrites a receipt another writer published.
fn restore_promotions(applied: &[Promotion]) -> Vec<String> {
    let mut issues = Vec::new();
    for promotion in applied.iter().rev() {
        match fs::symlink_metadata(&promotion.path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                issues.push(format!(
                    "left '{}': it is no longer a regular receipt",
                    promotion.path.display()
                ));
                continue;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                issues.push(format!(
                    "left '{}': the receipt disappeared",
                    promotion.path.display()
                ));
                continue;
            }
            Err(error) => {
                issues.push(format!(
                    "failed to inspect '{}': {error}",
                    promotion.path.display()
                ));
                continue;
            }
        }
        match fs::read(&promotion.path) {
            Ok(current) if current == promotion.bytes => {}
            Ok(_) => {
                issues.push(format!(
                    "left '{}': the receipt changed after its promotion",
                    promotion.path.display()
                ));
                continue;
            }
            Err(error) => {
                issues.push(format!(
                    "failed to read '{}': {error}",
                    promotion.path.display()
                ));
                continue;
            }
        }
        if let Err(error) = replace_receipt(&promotion.path, &promotion.original) {
            issues.push(format!(
                "failed to restore '{}': {error:#}",
                promotion.path.display()
            ));
        }
    }
    issues
}

/// A promotion changes only a receipt, so the package must still own its real
/// keg: a symlinked package directory or keg is a foreign path and is refused.
fn validate_promotion_keg(name: &str, version: &str, cellar: &Path) -> Result<()> {
    let package = cellar.join(name);
    match fs::symlink_metadata(&package) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to promote '{name} {version}': '{}' is a symlink",
            package.display()
        ),
        Ok(metadata) if !metadata.is_dir() => bail!(
            "refusing to promote '{name} {version}': '{}' is not a directory",
            package.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to promote '{name} {version}': '{}' is missing; remove it manually",
            package.display()
        ),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect '{}'", package.display()));
        }
    }

    let keg = package.join(version);
    match fs::symlink_metadata(&keg) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to promote '{name} {version}': its keg '{}' is a symlink",
            keg.display()
        ),
        Ok(metadata) if !metadata.is_dir() => bail!(
            "refusing to promote '{name} {version}': its keg '{}' is not a directory",
            keg.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to promote '{name} {version}': its keg '{}' is missing; remove it manually",
            keg.display()
        ),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", keg.display())),
    }
}

/// The exact relative target [`Store::commit`] stores for a keg member.
fn keg_link_target(name: &str, version: &str, member: &str) -> PathBuf {
    Path::new("..")
        .join(CELLAR)
        .join(name)
        .join(version)
        .join(BIN)
        .join(member)
}

/// Atomically publish a receipt; never replace an existing one.
fn write_receipt(directory: &Path, receipt: &Receipt) -> Result<()> {
    let target = directory.join(format!("{}.json", receipt.name));
    match fs::symlink_metadata(&target) {
        Ok(_) => bail!(
            "refusing to overwrite the existing receipt '{}'",
            target.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", target.display()));
        }
    }

    let bytes = encode_receipt(receipt)?;
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
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("failed to write '{}'", temporary.display()))?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to publish '{}'", target.display()));
    }
    sync_directory(directory);
    Ok(())
}

/// Encode a receipt exactly as it is stored on disk.
fn encode_receipt(receipt: &Receipt) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(receipt)
        .with_context(|| format!("failed to encode the receipt for '{}'", receipt.name))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Atomically replace one existing regular receipt with exact bytes.
///
/// The replacement is published from a temporary file in the same directory, so
/// a reader sees either the old bytes or the new bytes, never a partial receipt.
fn replace_receipt(path: &Path, bytes: &[u8]) -> Result<()> {
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
fn read_receipt(path: &Path) -> Result<Receipt> {
    read_receipt_with_bytes(path).map(|(receipt, _)| receipt)
}

/// Read one receipt together with its exact bytes, which a promotion restores on
/// rollback.
fn read_receipt_with_bytes(path: &Path) -> Result<(Receipt, Vec<u8>)> {
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

fn existing_receipt(path: &Path) -> Result<Option<Receipt>> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_receipt(path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Executable, regular, safely named members directly below `<stage>/bin`.
fn executable_bin_members(stage: &Path) -> Result<Vec<String>> {
    let bin = stage.join(BIN);
    let metadata = match fs::symlink_metadata(&bin) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", bin.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing stage '{}': its bin directory is a symlink",
            stage.display()
        );
    }
    if !metadata.is_dir() {
        return Ok(Vec::new());
    }

    let mut members = Vec::new();
    for entry in
        fs::read_dir(&bin).with_context(|| format!("failed to read '{}'", bin.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read an entry in '{}'", bin.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?
            .is_file()
        {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_safe_bin_name(&name) {
            continue;
        }
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?;
        if is_executable(&metadata) {
            members.push(name);
        }
    }
    members.sort();
    Ok(members)
}

/// Create the relative CLI link, never replacing an existing path.
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native CLI links are only supported on Unix",
        ))
    }
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

/// Validate a package name exactly as the archive layer does.
fn is_safe_package_name(name: &str) -> bool {
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

fn is_safe_version(version: &str) -> bool {
    if version.is_empty() || version.len() > MAX_VERSION_LEN || version.contains("..") {
        return false;
    }
    version
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
}

/// Bin members become single path components, so reject anything surprising.
fn is_safe_bin_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 || name.contains("..") {
        return false;
    }
    let bytes = name.as_bytes();
    (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'+' | b'-' | b'_' | b'.')
        })
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_receipt(receipt: &Receipt) -> Result<()> {
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

/// Validate a projected receipt graph as a whole.
///
/// This is pure: the caller supplies exactly the receipts the operation would
/// leave installed (commit adds, upgrade replaces, removal subtracts), and no
/// filesystem state is read. Every receipt's own fields are validated, package
/// names must be unique, and each recorded direct dependency must resolve to a
/// receipt with the same name, version, and digest. Errors name both the owner
/// and the affected dependency.
pub(super) fn validate_dependencies(receipts: &[Receipt]) -> Result<()> {
    let mut installed: HashMap<&str, &Receipt> = HashMap::with_capacity(receipts.len());
    for receipt in receipts {
        validate_receipt(receipt)?;
        if installed.insert(receipt.name.as_str(), receipt).is_some() {
            bail!(
                "package '{}' appears more than once in the projected receipt graph",
                receipt.name
            );
        }
    }
    for receipt in receipts {
        for dependency in &receipt.dependencies {
            let Some(actual) = installed.get(dependency.name.as_str()) else {
                bail!(
                    "'{}' depends on '{} {}' (sha256 {}), which the projected receipt graph does not contain; the dependency must be installed at that exact version and digest, or removed together with its dependent",
                    receipt.name,
                    dependency.name,
                    dependency.version,
                    dependency.sha256
                );
            };
            if actual.version != dependency.version || actual.sha256 != dependency.sha256 {
                bail!(
                    "'{}' depends on '{} {}' (sha256 {}), but the projected receipt graph records '{}' as {} (sha256 {}); the dependency must match exactly",
                    receipt.name,
                    dependency.name,
                    dependency.version,
                    dependency.sha256,
                    actual.name,
                    actual.version,
                    actual.sha256
                );
            }
        }
    }
    Ok(())
}

/// Serialize writers for one prefix with an exclusive lock file.
struct InstallLock {
    path: PathBuf,
    token: String,
}

impl InstallLock {
    fn acquire(prefix: &Path) -> Result<Self> {
        let path = state_dir(prefix).join(LOCK_FILE);
        let token = format!("pid={} nonce={}\n", std::process::id(), monotonic_nonce());
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(error) = file
                    .write_all(token.as_bytes())
                    .and_then(|()| file.sync_all())
                {
                    let _ = fs::remove_file(&path);
                    return Err(error)
                        .with_context(|| format!("failed to write lock '{}'", path.display()));
                }
                Ok(Self { path, token })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let owner = describe_lock(&path);
                bail!(
                    "another brewrs install holds the lock '{}' ({owner}); if no brewrs process is running, the lock is stale: remove '{}' and retry",
                    path.display(),
                    path.display()
                )
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to create lock '{}'", path.display()))
            }
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        // Only remove our own lock, and never follow a symlink planted here.
        // A replaced/stale lock is left for the explicit advice in `acquire`.
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.is_file() => {}
            _ => return,
        }
        if let Ok(contents) = fs::read(&self.path) {
            if contents == self.token.as_bytes() {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn describe_lock(path: &Path) -> String {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => "lock path is not a regular file".into(),
        Ok(_) => {
            let mut contents = String::new();
            match File::open(path).and_then(|file| file.take(4096).read_to_string(&mut contents)) {
                Ok(_) => {
                    let contents = contents.trim();
                    if contents.is_empty() {
                        "unreadable lock".into()
                    } else {
                        contents.to_string()
                    }
                }
                Err(_) => "unreadable lock".into(),
            }
        }
        Err(_) => "unreadable lock".into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerState {
    Missing,
    Valid,
}

fn marker_state(prefix: &Path) -> Result<MarkerState> {
    let path = marker_path(prefix);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': the {} marker is a symlink",
            prefix.display(),
            MARKER_FILE
        ),
        Ok(metadata) if !metadata.is_file() => bail!(
            "refusing '{}': the {} marker is not a regular file",
            prefix.display(),
            MARKER_FILE
        ),
        Ok(metadata) => {
            if metadata.len() > MARKER_CONTENT.len() as u64 {
                bail!(
                    "refusing '{}': the {} marker is not a brewrs marker",
                    prefix.display(),
                    MARKER_FILE
                );
            }
            let contents =
                fs::read(&path).with_context(|| format!("failed to read '{}'", path.display()))?;
            if contents == MARKER_CONTENT.as_bytes() {
                Ok(MarkerState::Valid)
            } else {
                bail!(
                    "refusing '{}': the {} marker does not belong to brewrs",
                    prefix.display(),
                    MARKER_FILE
                );
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(MarkerState::Missing),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

fn create_marker(prefix: &Path) -> Result<()> {
    let path = marker_path(prefix);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            if let Err(error) = file
                .write_all(MARKER_CONTENT.as_bytes())
                .and_then(|()| file.sync_all())
            {
                let _ = fs::remove_file(&path);
                return Err(error)
                    .with_context(|| format!("failed to initialize '{}'", path.display()));
            }
            sync_directory(prefix);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match marker_state(prefix)? {
            MarkerState::Valid => Ok(()),
            MarkerState::Missing => bail!(
                "refusing '{}': the marker disappeared while initializing it",
                prefix.display()
            ),
        },
        Err(error) => {
            Err(error).with_context(|| format!("failed to create marker '{}'", path.display()))
        }
    }
}

/// Require an owned directory, optionally creating it (and never a symlink).
fn ensure_directory(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': a brewrs-managed path may not be a symlink",
            path.display()
        ),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => bail!(
            "refusing '{}': a brewrs-managed path must be a directory",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !create {
                bail!(
                    "refusing '{}': the managed directory is missing",
                    path.display()
                );
            }
            fs::create_dir(path)
                .with_context(|| format!("failed to create '{}'", path.display()))?;
            ensure_directory(path, false)
        }
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Inspect a managed directory that is allowed to be absent.
///
/// Returns false only for a missing path; a symlink or file is an error.
fn check_optional_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': a brewrs-managed path may not be a symlink",
            path.display()
        ),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => bail!(
            "refusing '{}': a brewrs-managed path must be a directory",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Reject the filesystem root and any known Homebrew prefix, resolving
/// existing ancestors so a symlink cannot point a store into Homebrew.
fn reject_protected_prefix(prefix: &Path) -> Result<()> {
    let candidate = canonical_candidate(prefix)?;
    if candidate == Path::new("/") || candidate.parent().is_none() {
        bail!("refusing to use the filesystem root as a brewrs prefix");
    }
    for root in protected_prefixes() {
        let Ok(root) = canonical_candidate(&root) else {
            continue;
        };
        if candidate == root || candidate.starts_with(&root) {
            bail!(
                "refusing to use '{}' as a brewrs prefix: it is inside the Homebrew prefix '{}'; choose a dedicated directory such as ~/.brewrs",
                prefix.display(),
                root.display()
            );
        }
    }
    Ok(())
}

fn protected_prefixes() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["/opt/homebrew", "/usr/local", "/home/linuxbrew"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".linuxbrew"));
    }
    if let Some(prefix) = std::env::var_os("HOMEBREW_PREFIX") {
        let prefix = PathBuf::from(prefix);
        if prefix.is_absolute() {
            roots.push(prefix);
        }
    }
    roots
}

/// Resolve the path a prefix would occupy, following existing symlinks and
/// dangling ones, without requiring the missing tail to exist.
fn canonical_candidate(path: &Path) -> Result<PathBuf> {
    canonical_candidate_inner(path, 0)
}

fn canonical_candidate_inner(path: &Path, depth: usize) -> Result<PathBuf> {
    if depth > 64 {
        bail!("could not resolve '{}': too many symlinks", path.display());
    }
    let mut probe = path;
    let mut tail: Vec<&OsStr> = Vec::new();
    loop {
        match fs::symlink_metadata(probe) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // `canonicalize` gives up on dangling links, so resolve the
                // link target ourselves and keep inspecting the tail.
                let target = fs::read_link(probe)
                    .with_context(|| format!("could not read link '{}'", probe.display()))?;
                let target = if target.is_absolute() {
                    target
                } else {
                    probe
                        .parent()
                        .with_context(|| format!("could not resolve '{}'", path.display()))?
                        .join(target)
                };
                let mut base = canonical_candidate_inner(&target, depth + 1)?;
                append_tail(&mut base, &tail);
                return Ok(base);
            }
            Ok(_) => {
                let mut base = probe
                    .canonicalize()
                    .with_context(|| format!("could not resolve '{}'", path.display()))?;
                append_tail(&mut base, &tail);
                return Ok(base);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(name) = probe.file_name() else {
                    bail!("could not resolve '{}': {error}", path.display());
                };
                tail.push(name);
                probe = probe
                    .parent()
                    .with_context(|| format!("could not resolve '{}'", path.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not resolve '{}'", path.display()));
            }
        }
    }
}

fn append_tail(base: &mut PathBuf, tail: &[&OsStr]) {
    for part in tail.iter().rev() {
        if *part == OsStr::new("..") {
            base.pop();
        } else if *part != OsStr::new(".") {
            base.push(*part);
        }
    }
}

fn sync_directory(directory: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(directory) {
        if let Err(error) = handle.sync_all() {
            log::debug!("failed to flush '{}': {error}", directory.display());
        }
    }
    #[cfg(not(unix))]
    let _ = directory;
}

fn monotonic_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn marker_path(prefix: &Path) -> PathBuf {
    prefix.join(MARKER_FILE)
}

fn meta_dir(prefix: &Path) -> PathBuf {
    prefix.join(META)
}

fn state_dir(prefix: &Path) -> PathBuf {
    meta_dir(prefix).join(STATE)
}

fn receipts_dir(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(RECEIPTS)
}

fn stages_dir(prefix: &Path) -> PathBuf {
    meta_dir(prefix).join(STAGES)
}

fn cellar_dir(prefix: &Path) -> PathBuf {
    prefix.join(CELLAR)
}

fn bin_dir(prefix: &Path) -> PathBuf {
    prefix.join(BIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{TempDir, tempdir};

    fn digest() -> String {
        "a".repeat(64)
    }

    fn receipt(name: &str, version: &str, requested: bool) -> Receipt {
        Receipt {
            name: name.into(),
            version: version.into(),
            sha256: digest(),
            requested,
            dependencies: Vec::new(),
        }
    }

    fn dependency(receipt: &Receipt) -> Dependency {
        Dependency {
            name: receipt.name.clone(),
            version: receipt.version.clone(),
            sha256: receipt.sha256.clone(),
        }
    }

    fn new_store() -> (TempDir, Store) {
        let temp = tempdir().expect("temporary directory");
        let prefix = temp.path().join("prefix");
        let store = Store::open(&prefix, true).expect("create store");
        (temp, store)
    }

    fn prepare(store: &Store, name: &str, version: &str, bins: &[(&str, bool)]) -> Prepared {
        let stage = store.stage().expect("stage directory");
        let bin = stage.path().join(BIN);
        fs::create_dir_all(&bin).expect("create staged bin");
        for (member, executable) in bins {
            let path = bin.join(member);
            fs::write(&path, b"#!/bin/sh\n").expect("write staged member");
            fs::set_permissions(
                &path,
                fs::Permissions::from_mode(if *executable { 0o755 } else { 0o644 }),
            )
            .expect("set staged mode");
        }
        let lib = stage.path().join("lib");
        fs::create_dir_all(&lib).expect("create staged lib");
        fs::write(lib.join(format!("{name}.dylib")), b"payload").expect("write staged payload");
        Prepared {
            receipt: receipt(name, version, true),
            stage,
        }
    }

    fn install(store: &Store, name: &str, version: &str, bins: &[(&str, bool)]) -> Receipt {
        let prepared = prepare(store, name, version, bins);
        let receipt = prepared.receipt.clone();
        store.commit(vec![prepared]).expect("install package");
        receipt
    }

    /// Install a package as an unrequested dependency, as a commit of a
    /// dependency-first plan would.
    fn install_unrequested(
        store: &Store,
        name: &str,
        version: &str,
        bins: &[(&str, bool)],
    ) -> Receipt {
        let mut prepared = prepare(store, name, version, bins);
        prepared.receipt.requested = false;
        let receipt = prepared.receipt.clone();
        store.commit(vec![prepared]).expect("install dependency");
        receipt
    }

    fn prepare_with(
        store: &Store,
        name: &str,
        version: &str,
        bins: &[(&str, bool)],
        dependencies: &[Dependency],
    ) -> Prepared {
        let mut prepared = prepare(store, name, version, bins);
        prepared.receipt.dependencies = dependencies.to_vec();
        prepared
    }

    fn install_with_dependency(
        store: &Store,
        name: &str,
        version: &str,
        dependency_on: &Receipt,
        bins: &[(&str, bool)],
    ) -> Receipt {
        let prepared = prepare_with(store, name, version, bins, &[dependency(dependency_on)]);
        let receipt = prepared.receipt.clone();
        store
            .commit(vec![prepared])
            .expect("install dependent package");
        receipt
    }

    fn read_only(store: &Store) -> Store {
        Store::open(store.prefix(), false).expect("read-only reopen")
    }

    /// Every path below `root`, with link targets and file bytes, for exact
    /// before/after comparisons.
    fn tree(root: &Path) -> Vec<(PathBuf, String)> {
        fn walk(root: &Path, directory: &Path, out: &mut Vec<(PathBuf, String)>) {
            for entry in fs::read_dir(directory).expect("read directory") {
                let path = entry.expect("entry").path();
                let metadata = fs::symlink_metadata(&path).expect("metadata");
                let relative = path.strip_prefix(root).expect("relative").to_path_buf();
                if metadata.file_type().is_symlink() {
                    out.push((
                        relative,
                        format!("link:{}", fs::read_link(&path).expect("link").display()),
                    ));
                } else if metadata.is_dir() {
                    out.push((relative, "directory".into()));
                    walk(root, &path, out);
                } else {
                    out.push((
                        relative,
                        format!(
                            "file:{}",
                            String::from_utf8_lossy(&fs::read(&path).expect("file")).escape_debug()
                        ),
                    ));
                }
            }
        }
        let mut entries = Vec::new();
        walk(root, root, &mut entries);
        entries.sort();
        entries
    }

    #[test]
    fn read_only_binding_of_an_absent_prefix_is_empty_and_writes_nothing() {
        let temp = tempdir().expect("temporary directory");
        let prefix = temp.path().join("absent");
        let store = Store::open(&prefix, false).expect("bind absent prefix");

        assert_eq!(store.prefix(), prefix);
        assert!(store.installed().expect("installed").is_empty());
        assert!(!prefix.exists());
        assert!(store.stage().is_err());
        assert!(store.commit(Vec::new()).is_err());
        assert!(!prefix.exists());
    }

    #[test]
    fn creates_the_marker_and_every_owned_directory() {
        let temp = tempdir().expect("temporary directory");
        let prefix = temp.path().join("prefix");
        let store = Store::open(&prefix, true).expect("create store");

        assert_eq!(store.prefix(), prefix);
        assert_eq!(
            fs::read(prefix.join(MARKER_FILE)).expect("marker"),
            MARKER_CONTENT.as_bytes()
        );
        for directory in [
            cellar_dir(&prefix),
            bin_dir(&prefix),
            meta_dir(&prefix),
            state_dir(&prefix),
            receipts_dir(&prefix),
            stages_dir(&prefix),
        ] {
            assert!(
                directory.is_dir(),
                "{} should be a directory",
                directory.display()
            );
        }
        assert!(store.installed().expect("installed").is_empty());
        drop(store);

        let reopened = Store::open(&prefix, false).expect("read-only reopen");
        assert!(reopened.installed().expect("installed").is_empty());
    }

    #[test]
    fn initializes_an_existing_empty_directory() {
        let temp = tempdir().expect("temporary directory");
        let prefix = temp.path().join("empty");
        fs::create_dir(&prefix).expect("create empty prefix");

        Store::open(&prefix, true).expect("adopt empty directory");
        assert!(prefix.join(MARKER_FILE).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_prefix() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temporary directory");
        let real = temp.path().join("real");
        fs::create_dir(&real).expect("create real prefix");
        let link = temp.path().join("link");
        symlink(&real, &link).expect("symlink prefix");

        assert!(Store::open(&link, true).is_err());
        assert!(Store::open(&link, false).is_err());
        assert!(!real.join(MARKER_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_managed_directories() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = new_store();
        let prefix = store.prefix().to_path_buf();
        drop(store);
        let outside = tempdir().expect("outside directory");
        fs::remove_dir_all(prefix.join(META)).expect("remove state directory");
        symlink(outside.path(), prefix.join(META)).expect("symlink state directory");

        assert!(Store::open(&prefix, true).is_err());
        let read_only = Store::open(&prefix, false).expect("bind marked prefix");
        assert!(read_only.installed().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolves_dangling_symlinks_when_checking_protection() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temporary directory");
        let target = temp.path().join("outside").join("missing");
        let link = temp.path().join("link");
        symlink(&target, &link).expect("dangling symlink");

        let resolved = canonical_candidate(&link.join("sub")).expect("resolve candidate");
        assert_eq!(
            resolved,
            temp.path()
                .canonicalize()
                .expect("canonical temp")
                .join("outside")
                .join("missing")
                .join("sub")
        );
    }

    #[test]
    fn refuses_a_nonempty_unowned_prefix_without_touching_it() {
        let temp = tempdir().expect("temporary directory");
        let prefix = temp.path().join("prefix");
        fs::create_dir(&prefix).expect("create prefix");
        fs::write(prefix.join("keep"), b"x").expect("write foreign file");

        let error = Store::open(&prefix, true).expect_err("unowned prefix must fail");
        assert!(format!("{error:#}").contains("not empty"), "{error:#}");
        assert!(prefix.join("keep").exists());
        assert!(!prefix.join(MARKER_FILE).exists());
    }

    #[test]
    fn refuses_known_homebrew_prefixes() {
        assert!(Store::open(Path::new("/opt/homebrew"), true).is_err());
        assert!(Store::open(Path::new("/usr/local"), true).is_err());
        assert!(Store::open(Path::new("/home/linuxbrew/.linuxbrew"), true).is_err());

        let unique = format!("brewrs-store-test-{}", std::process::id());
        let candidate = Path::new("/usr/local").join(unique);
        let error = Store::open(&candidate, true).expect_err("Homebrew descendant must fail");
        assert!(format!("{error:#}").contains("Homebrew"), "{error:#}");
        assert!(!candidate.exists());
    }

    #[test]
    fn refuses_a_foreign_marker() {
        let (_temp, store) = new_store();
        fs::write(store.prefix().join(MARKER_FILE), b"foreign\n").expect("replace marker");
        assert!(Store::open(store.prefix(), false).is_err());
        assert!(Store::open(store.prefix(), true).is_err());
    }

    #[test]
    fn commit_installs_files_links_and_a_receipt() {
        let (_temp, store) = new_store();
        let prepared = prepare(
            &store,
            "hello",
            "1.0",
            &[
                ("hello", true),
                ("readme", false),
                (".hidden", true),
                ("bad name", true),
            ],
        );
        let stage_root = prepared.stage.path().to_path_buf();
        let expected = prepared.receipt.clone();

        store.commit(vec![prepared]).expect("commit package");

        assert!(
            !stage_root.exists(),
            "the stage must be moved into the Cellar"
        );
        let keg = store.prefix().join(CELLAR).join("hello").join("1.0");
        assert_eq!(
            fs::read(keg.join("bin").join("hello")).expect("keg binary"),
            b"#!/bin/sh\n"
        );
        assert_eq!(
            fs::read(keg.join("lib").join("hello.dylib")).expect("keg payload"),
            b"payload"
        );

        let link = store.prefix().join(BIN).join("hello");
        assert!(
            fs::symlink_metadata(&link)
                .expect("link metadata")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&link).expect("link target"),
            Path::new("../Cellar/hello/1.0/bin/hello")
        );
        for skipped in ["readme", ".hidden", "bad name"] {
            assert!(
                fs::symlink_metadata(store.prefix().join(BIN).join(skipped)).is_err(),
                "{skipped} must not be linked"
            );
        }

        assert_eq!(store.installed().expect("installed"), vec![expected]);
        assert!(
            store
                .prefix()
                .join(META)
                .join(STATE)
                .join(RECEIPTS)
                .join("hello.json")
                .is_file()
        );

        let prefix = store.prefix().to_path_buf();
        drop(store);
        let reopened = Store::open(&prefix, false).expect("read-only reopen");
        assert_eq!(reopened.installed().expect("installed").len(), 1);
        assert!(reopened.stage().is_err());
    }

    #[test]
    fn installed_reports_receipts_sorted_by_name() {
        let (_temp, store) = new_store();
        let zeta = prepare(&store, "zeta", "2.0", &[("zeta", true)]);
        let mut alpha = prepare(&store, "alpha", "1.0", &[("alpha", true)]);
        alpha.receipt.requested = false;

        store.commit(vec![zeta, alpha]).expect("commit packages");
        let names: Vec<_> = store
            .installed()
            .expect("installed")
            .into_iter()
            .map(|receipt| (receipt.name, receipt.requested))
            .collect();
        assert_eq!(
            names,
            vec![("alpha".to_string(), false), ("zeta".to_string(), true)]
        );
    }

    #[test]
    fn refuses_to_overwrite_an_existing_bin_path() {
        let (_temp, store) = new_store();
        let occupied = store.prefix().join(BIN).join("tool");
        fs::write(&occupied, b"keep").expect("occupy bin name");

        let error = store
            .commit(vec![prepare(&store, "alpha", "1.0", &[("tool", true)])])
            .expect_err("must not overwrite");
        assert!(format!("{error:#}").contains("already exists"), "{error:#}");
        assert_eq!(fs::read(&occupied).expect("foreign file"), b"keep");
        assert!(store.installed().expect("installed").is_empty());
        assert!(!store.prefix().join(CELLAR).join("alpha").exists());
    }

    #[test]
    fn refuses_duplicate_bin_names_across_one_commit() {
        let (_temp, store) = new_store();
        let error = store
            .commit(vec![
                prepare(&store, "alpha", "1.0", &[("shared", true)]),
                prepare(&store, "beta", "1.0", &[("shared", true)]),
            ])
            .expect_err("cross-package collision must fail");
        assert!(
            format!("{error:#}").contains("more than one package"),
            "{error:#}"
        );
        assert!(store.installed().expect("installed").is_empty());
        assert!(
            fs::read_dir(store.prefix().join(BIN))
                .expect("read bin")
                .next()
                .is_none()
        );
    }

    #[test]
    fn refuses_duplicate_package_names_in_one_commit() {
        let (_temp, store) = new_store();
        let error = store
            .commit(vec![
                prepare(&store, "hello", "1.0", &[("hello", true)]),
                prepare(&store, "hello", "1.0", &[("other", true)]),
            ])
            .expect_err("duplicate package name must fail");
        assert!(format!("{error:#}").contains("more than once"), "{error:#}");
        assert!(store.installed().expect("installed").is_empty());
    }

    #[test]
    fn refuses_to_replace_an_installed_version() {
        let (_temp, store) = new_store();
        store
            .commit(vec![prepare(&store, "hello", "1.0", &[("hello", true)])])
            .expect("first install");

        let error = store
            .commit(vec![prepare(&store, "hello", "2.0", &[("hello", true)])])
            .expect_err("different version must fail");
        assert!(
            format!("{error:#}").contains("already installed"),
            "{error:#}"
        );

        let installed = store.installed().expect("installed");
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].version, "1.0");
        assert!(
            store
                .prefix()
                .join(CELLAR)
                .join("hello")
                .join("1.0")
                .is_dir()
        );
        assert!(
            !store
                .prefix()
                .join(CELLAR)
                .join("hello")
                .join("2.0")
                .exists()
        );
    }

    #[test]
    fn refuses_the_same_version_and_leaves_idempotence_to_the_caller() {
        let (_temp, store) = new_store();
        store
            .commit(vec![prepare(&store, "hello", "1.0", &[("hello", true)])])
            .expect("first install");

        let error = store
            .commit(vec![prepare(&store, "hello", "1.0", &[("hello", true)])])
            .expect_err("same version must fail defensively");
        assert!(
            format!("{error:#}").contains("already installed"),
            "{error:#}"
        );
        assert_eq!(store.installed().expect("installed").len(), 1);
    }

    #[test]
    fn validates_receipts_before_writing_anything() {
        let (_temp, store) = new_store();
        let cases = [
            ("../evil".to_string(), "1.0".to_string(), digest()),
            ("hello".to_string(), "../1".to_string(), digest()),
            (
                "hello".to_string(),
                "1.0".to_string(),
                digest().to_uppercase(),
            ),
            ("hello".to_string(), "1.0".to_string(), "a".repeat(63)),
        ];
        for (name, version, sha256) in cases {
            let mut prepared = prepare(&store, "placeholder", "1.0", &[]);
            prepared.receipt.name = name;
            prepared.receipt.version = version;
            prepared.receipt.sha256 = sha256;
            assert!(store.commit(vec![prepared]).is_err());
        }
        assert!(store.installed().expect("installed").is_empty());
        assert!(
            fs::read_dir(store.prefix().join(CELLAR))
                .expect("read cellar")
                .next()
                .is_none()
        );
    }

    #[test]
    fn refuses_a_stage_owned_by_another_directory() {
        let (_temp, store) = new_store();
        let foreign = tempdir().expect("foreign temporary directory");
        fs::create_dir_all(foreign.path().join(BIN)).expect("foreign bin");

        let error = store
            .commit(vec![Prepared {
                receipt: receipt("hello", "1.0", true),
                stage: foreign,
            }])
            .expect_err("foreign stage must fail");
        assert!(format!("{error:#}").contains("not owned"), "{error:#}");
        assert!(store.installed().expect("installed").is_empty());
    }

    #[test]
    fn lock_blocks_writers_and_explains_stale_locks() {
        let (_temp, store) = new_store();
        let lock = store.prefix().join(META).join(STATE).join(LOCK_FILE);
        fs::write(&lock, b"pid=1 nonce=stale\n").expect("write stale lock");

        let error = store
            .commit(vec![prepare(&store, "hello", "1.0", &[])])
            .expect_err("held lock must fail");
        let message = format!("{error:#}");
        assert!(message.contains("stale"), "{message}");
        assert!(message.contains("remove"), "{message}");
        assert!(lock.exists());

        fs::remove_file(&lock).expect("clear stale lock");
        store
            .commit(vec![prepare(&store, "hello", "1.0", &[])])
            .expect("commit after clearing lock");
        assert!(!lock.exists(), "a successful commit drops its lock");
    }

    #[test]
    fn rolls_back_every_owned_path_when_a_later_package_fails() {
        let (_temp, store) = new_store();
        let first = prepare(&store, "alpha", "1.0", &[("alpha", true)]);
        let second = prepare(&store, "beta", "1.0", &[("beta", true)]);

        let error = store
            .commit_inner(vec![first, second], Some(1))
            .expect_err("injected failure");
        assert!(format!("{error:#}").contains("injected"), "{error:#}");

        assert!(store.installed().expect("installed").is_empty());
        assert!(!store.prefix().join(CELLAR).join("alpha").exists());
        assert!(!store.prefix().join(CELLAR).join("beta").exists());
        assert!(
            fs::read_dir(store.prefix().join(BIN))
                .expect("read bin")
                .next()
                .is_none()
        );
        assert!(
            !store
                .prefix()
                .join(META)
                .join(STATE)
                .join(LOCK_FILE)
                .exists()
        );
    }

    #[test]
    fn removal_removes_only_the_selected_owned_paths() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = new_store();
        let alpha = install(
            &store,
            "alpha",
            "1.0",
            &[("alpha", true), ("alpha-tool", true)],
        );
        let beta = install(&store, "beta", "1.0", &[("beta", true)]);
        let prefix = store.prefix().to_path_buf();

        fs::write(prefix.join("keep.txt"), b"keep").expect("foreign file");
        fs::write(prefix.join(BIN).join("note"), b"keep").expect("foreign bin file");
        let foreign = prefix.join(BIN).join("alpha-tool");
        fs::remove_file(&foreign).expect("drop owned link");
        symlink("/usr/bin/true", &foreign).expect("foreign link");

        let before = tree(&prefix);
        read_only(&store)
            .remove(std::slice::from_ref(&alpha))
            .expect("remove alpha");

        let mut expected = before;
        expected.retain(|(path, _)| {
            path.as_path() != Path::new(".brewrs/state/receipts/alpha.json")
                && !path.starts_with("Cellar/alpha")
                && path.as_path() != Path::new("bin/alpha")
        });
        assert_eq!(tree(&prefix), expected);
        assert_eq!(
            read_only(&store).installed().expect("installed"),
            vec![beta]
        );
        assert_eq!(
            fs::read_link(&foreign).expect("foreign link"),
            Path::new("/usr/bin/true")
        );
        assert_eq!(fs::read(prefix.join("keep.txt")).expect("keep"), b"keep");
        assert_eq!(
            fs::read(prefix.join(MARKER_FILE)).expect("marker"),
            MARKER_CONTENT.as_bytes()
        );
        assert!(
            fs::read_dir(prefix.join(META).join(STAGES))
                .expect("stages")
                .next()
                .is_none(),
            "a successful removal leaves no quarantine"
        );
        assert!(!prefix.join(META).join(STATE).join(LOCK_FILE).exists());
    }

    #[test]
    fn removal_preflights_every_target_before_moving_anything() {
        let (_temp, store) = new_store();
        let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
        let beta = install(&store, "beta", "1.0", &[("beta", true)]);
        let prefix = store.prefix().to_path_buf();
        fs::remove_dir_all(prefix.join(CELLAR).join("beta").join("1.0")).expect("damage beta");
        let before = tree(&prefix);

        let error = read_only(&store)
            .remove(&[alpha, beta])
            .expect_err("a missing keg must fail preflight");
        assert!(format!("{error:#}").contains("beta"), "{error:#}");
        assert_eq!(tree(&prefix), before);
        assert_eq!(read_only(&store).installed().expect("installed").len(), 2);
    }

    #[test]
    fn removal_rejects_a_stale_plan() {
        let (_temp, store) = new_store();
        let mut stale = install(&store, "hello", "1.0", &[("hello", true)]);
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        stale.version = "2.0".into();
        let error = read_only(&store)
            .remove(&[stale])
            .expect_err("stale receipt must fail");
        assert!(format!("{error:#}").contains("re-read"), "{error:#}");
        assert_eq!(tree(&prefix), before);
    }

    #[test]
    fn removal_rejects_duplicate_targets() {
        let (_temp, store) = new_store();
        let hello = install(&store, "hello", "1.0", &[("hello", true)]);
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        let error = read_only(&store)
            .remove(&[hello.clone(), hello])
            .expect_err("duplicate target must fail");
        assert!(format!("{error:#}").contains("more than once"), "{error:#}");
        assert_eq!(tree(&prefix), before);
    }

    #[test]
    fn removal_handles_multiple_targets_in_one_call() {
        let (_temp, store) = new_store();
        let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
        let beta = install(&store, "beta", "1.0", &[("beta", true)]);
        let prefix = store.prefix().to_path_buf();

        read_only(&store)
            .remove(&[alpha, beta])
            .expect("remove both packages");

        assert!(read_only(&store).installed().expect("installed").is_empty());
        for directory in [CELLAR, BIN] {
            assert!(
                fs::read_dir(prefix.join(directory))
                    .expect("managed directory")
                    .next()
                    .is_none(),
                "{directory} should be empty"
            );
        }
        assert!(
            fs::read_dir(prefix.join(META).join(STAGES))
                .expect("stages")
                .next()
                .is_none()
        );
        assert!(prefix.join(MARKER_FILE).is_file());
    }

    #[test]
    fn removal_honours_the_install_writer_lock() {
        let (_temp, store) = new_store();
        let hello = install(&store, "hello", "1.0", &[("hello", true)]);
        let prefix = store.prefix().to_path_buf();
        let lock = prefix.join(META).join(STATE).join(LOCK_FILE);
        fs::write(&lock, b"pid=1 nonce=stale\n").expect("write stale lock");
        let before = tree(&prefix);

        let error = read_only(&store)
            .remove(std::slice::from_ref(&hello))
            .expect_err("a held lock must fail");
        assert!(format!("{error:#}").contains("stale"), "{error:#}");
        assert_eq!(tree(&prefix), before);
    }

    #[test]
    fn removal_removes_a_dangling_owned_link_and_allows_reinstall() {
        let (_temp, store) = new_store();
        let hello = install(&store, "hello", "1.0", &[("hello", true)]);
        let prefix = store.prefix().to_path_buf();
        let link = prefix.join(BIN).join("hello");
        fs::remove_file(
            prefix
                .join(CELLAR)
                .join("hello")
                .join("1.0")
                .join(BIN)
                .join("hello"),
        )
        .expect("delete the keg member");
        assert!(
            fs::symlink_metadata(&link)
                .expect("owned link")
                .file_type()
                .is_symlink()
        );

        read_only(&store)
            .remove(std::slice::from_ref(&hello))
            .expect("remove the dangling link");

        assert!(fs::symlink_metadata(&link).is_err());
        assert!(read_only(&store).installed().expect("installed").is_empty());
        // A reinstall must not collide with the old dangling link.
        install(&store, "hello", "1.0", &[("hello", true)]);
        assert_eq!(read_only(&store).installed().expect("installed").len(), 1);
        assert_eq!(
            fs::read_link(prefix.join(BIN).join("hello")).expect("new link"),
            Path::new("../Cellar/hello/1.0/bin/hello")
        );
    }

    #[test]
    fn removal_rolls_back_every_move_when_apply_fails() {
        let (_temp, store) = new_store();
        let hello = install(
            &store,
            "hello",
            "1.0",
            &[("hello", true), ("hello-tool", true)],
        );
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        // Steps are receipt, each link, then the keg; failing before the keg
        // makes the rollback restore several paths.
        let error = store
            .remove_inner(std::slice::from_ref(&hello), Some(3))
            .expect_err("injected removal failure");
        assert!(format!("{error:#}").contains("injected"), "{error:#}");
        assert_eq!(tree(&prefix), before);
        assert_eq!(
            read_only(&store).installed().expect("installed"),
            vec![hello]
        );
    }

    #[test]
    fn removal_rolls_back_a_pruned_package_directory() {
        let (_temp, store) = new_store();
        let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
        let beta = install(&store, "beta", "1.0", &[("beta", true)]);
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        // Alpha's steps are receipt, link, keg, prune; beta then moves its
        // receipt. Failing at beta's link forces alpha's pruned directory to
        // be recreated before alpha's keg can return.
        let error = store
            .remove_inner(&[alpha, beta], Some(5))
            .expect_err("injected removal failure");
        assert!(format!("{error:#}").contains("injected"), "{error:#}");
        assert_eq!(tree(&prefix), before);
        assert_eq!(read_only(&store).installed().expect("installed").len(), 2);
    }

    #[test]
    fn removal_keeps_foreign_cellar_entries_and_the_marker() {
        let (_temp, store) = new_store();
        let hello = install(&store, "hello", "1.0", &[("hello", true)]);
        let prefix = store.prefix().to_path_buf();
        let foreign = prefix.join(CELLAR).join("hello").join("1.0-notes");
        fs::create_dir(&foreign).expect("foreign cellar entry");
        fs::write(foreign.join("keep"), b"keep").expect("foreign payload");

        read_only(&store)
            .remove(std::slice::from_ref(&hello))
            .expect("remove hello");

        assert_eq!(fs::read(foreign.join("keep")).expect("keep"), b"keep");
        assert!(prefix.join(CELLAR).join("hello").is_dir());
        assert!(!prefix.join(CELLAR).join("hello").join("1.0").exists());
        assert_eq!(
            fs::read(prefix.join(MARKER_FILE)).expect("marker"),
            MARKER_CONTENT.as_bytes()
        );
        assert!(read_only(&store).installed().expect("installed").is_empty());
    }

    #[test]
    fn removal_requires_a_marker_and_never_initializes_a_prefix() {
        let temp = tempdir().expect("temporary directory");
        let target = receipt("hello", "1.0", true);

        let absent = temp.path().join("absent");
        let error = Store::open(&absent, false)
            .expect("bind absent prefix")
            .remove(std::slice::from_ref(&target))
            .expect_err("absent prefix must fail");
        assert!(
            format!("{error:#}").contains("not an initialized"),
            "{error:#}"
        );
        assert!(!absent.exists());

        let unowned = temp.path().join("unowned");
        fs::create_dir(&unowned).expect("create unowned prefix");
        fs::write(unowned.join("keep"), b"keep").expect("foreign file");
        let error = Store::open(&unowned, false)
            .expect("bind unowned prefix")
            .remove(&[target])
            .expect_err("unowned prefix must fail");
        assert!(
            format!("{error:#}").contains("not an initialized"),
            "{error:#}"
        );
        assert_eq!(fs::read(unowned.join("keep")).expect("keep"), b"keep");
        assert!(!unowned.join(MARKER_FILE).exists());
        assert!(!unowned.join(META).exists());
    }

    #[test]
    fn removal_refuses_a_prefix_inside_homebrew() {
        let candidate =
            Path::new("/usr/local").join(format!("brewrs-remove-test-{}", std::process::id()));
        let store = Store {
            prefix: candidate.clone(),
            state: RootState::Owned,
            writable: false,
        };
        let error = store
            .remove(&[receipt("hello", "1.0", true)])
            .expect_err("Homebrew prefix must be refused");
        assert!(format!("{error:#}").contains("Homebrew"), "{error:#}");
        assert!(!candidate.exists());
    }

    #[test]
    fn open_existing_requires_ownership_and_never_initializes() {
        let temp = tempdir().expect("temporary directory");

        let absent = temp.path().join("absent");
        let error = Store::open_existing(&absent).expect_err("absent prefix must fail");
        assert!(
            format!("{error:#}").contains("not an initialized"),
            "{error:#}"
        );
        assert!(!absent.exists());

        let unowned = temp.path().join("unowned");
        fs::create_dir(&unowned).expect("create unowned prefix");
        fs::write(unowned.join("keep"), b"keep").expect("foreign file");
        let error = Store::open_existing(&unowned).expect_err("unowned prefix must fail");
        assert!(
            format!("{error:#}").contains("not an initialized"),
            "{error:#}"
        );
        assert_eq!(fs::read(unowned.join("keep")).expect("keep"), b"keep");
        assert!(!unowned.join(MARKER_FILE).exists());
        assert!(!unowned.join(META).exists());

        let (_temp, store) = new_store();
        let prefix = store.prefix().to_path_buf();
        let writable = Store::open_existing(&prefix).expect("writable binding");
        drop(store);
        writable.stage().expect("staging needs a writable binding");

        fs::remove_dir(prefix.join(BIN)).expect("remove managed directory");
        let error = Store::open_existing(&prefix).expect_err("missing directory must fail");
        assert!(format!("{error:#}").contains("missing"), "{error:#}");
        assert!(!prefix.join(BIN).exists());
    }

    #[test]
    fn upgrade_replaces_the_old_receipt_keg_and_owned_links() {
        let (_temp, store) = new_store();
        let old = install(
            &store,
            "hello",
            "1.0",
            &[("hello", true), ("hello-tool", true)],
        );
        let prefix = store.prefix().to_path_buf();
        fs::write(prefix.join("keep.txt"), b"keep").expect("foreign file");
        fs::write(prefix.join(BIN).join("note"), b"keep").expect("foreign bin file");

        let replacement = prepare(
            &store,
            "hello",
            "2.0",
            &[("hello", true), ("hello-next", true)],
        );
        let expected = replacement.receipt.clone();
        let writable = Store::open_existing(&prefix).expect("writable binding");
        writable
            .upgrade(vec![(old, replacement)])
            .expect("upgrade hello");

        assert_eq!(writable.installed().expect("installed"), vec![expected]);
        assert!(
            !prefix.join(CELLAR).join("hello").join("1.0").exists(),
            "the old keg must be gone"
        );
        assert_eq!(
            fs::read(
                prefix
                    .join(CELLAR)
                    .join("hello")
                    .join("2.0")
                    .join("lib")
                    .join("hello.dylib")
            )
            .expect("new keg payload"),
            b"payload"
        );
        assert_eq!(
            fs::read_link(prefix.join(BIN).join("hello")).expect("new link"),
            Path::new("../Cellar/hello/2.0/bin/hello")
        );
        assert!(
            fs::symlink_metadata(prefix.join(BIN).join("hello-tool")).is_err(),
            "an old owned link must not survive"
        );
        assert_eq!(
            fs::read_link(prefix.join(BIN).join("hello-next")).expect("new tool"),
            Path::new("../Cellar/hello/2.0/bin/hello-next")
        );
        assert_eq!(fs::read(prefix.join("keep.txt")).expect("keep"), b"keep");
        assert_eq!(
            fs::read(prefix.join(BIN).join("note")).expect("keep bin"),
            b"keep"
        );
        assert!(
            fs::read_dir(prefix.join(META).join(STAGES))
                .expect("stages")
                .next()
                .is_none(),
            "a successful upgrade leaves no quarantine"
        );
        assert!(!prefix.join(META).join(STATE).join(LOCK_FILE).exists());
    }

    #[test]
    fn upgrade_rejects_a_stale_or_missing_old_receipt() {
        let (_temp, store) = new_store();
        let old = install(&store, "hello", "1.0", &[("hello", true)]);
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        let mut stale = old.clone();
        stale.version = "0.9".into();
        let error = store
            .upgrade(vec![(
                stale,
                prepare(&store, "hello", "2.0", &[("hello", true)]),
            )])
            .expect_err("stale old receipt must fail");
        assert!(format!("{error:#}").contains("re-read"), "{error:#}");
        assert_eq!(tree(&prefix), before);

        let receipt_path = prefix
            .join(META)
            .join(STATE)
            .join(RECEIPTS)
            .join("hello.json");
        fs::remove_file(&receipt_path).expect("remove the receipt");
        let missing = tree(&prefix);
        let error = store
            .upgrade(vec![(
                old,
                prepare(&store, "hello", "2.0", &[("hello", true)]),
            )])
            .expect_err("missing old receipt must fail");
        assert!(
            format!("{error:#}").contains("no longer exists"),
            "{error:#}"
        );
        assert_eq!(tree(&prefix), missing);
    }

    #[test]
    fn upgrade_requires_matching_name_requested_status_and_a_new_version() {
        let (_temp, store) = new_store();
        let old = install(&store, "hello", "1.0", &[("hello", true)]);
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        let mut renamed = prepare(&store, "hello", "2.0", &[("hello", true)]);
        renamed.receipt.name = "other".into();
        let error = store
            .upgrade(vec![(old.clone(), renamed)])
            .expect_err("name mismatch must fail");
        assert!(format!("{error:#}").contains("staged package"), "{error:#}");
        assert_eq!(tree(&prefix), before);

        let mut unrequested = prepare(&store, "hello", "2.0", &[("hello", true)]);
        unrequested.receipt.requested = false;
        let error = store
            .upgrade(vec![(old.clone(), unrequested)])
            .expect_err("requested mismatch must fail");
        assert!(
            format!("{error:#}").contains("requested status"),
            "{error:#}"
        );
        assert_eq!(tree(&prefix), before);

        let error = store
            .upgrade(vec![(
                old,
                prepare(&store, "hello", "1.0", &[("hello", true)]),
            )])
            .expect_err("same version must fail");
        assert!(
            format!("{error:#}").contains("already installed"),
            "{error:#}"
        );
        assert_eq!(tree(&prefix), before);
    }

    #[test]
    fn upgrade_rejects_foreign_alternate_version_and_batch_collisions() {
        let (_temp, store) = new_store();
        let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
        let beta = install(&store, "beta", "1.0", &[("beta", true)]);
        let prefix = store.prefix().to_path_buf();

        let foreign = prefix.join(BIN).join("tool");
        fs::write(&foreign, b"keep").expect("foreign bin file");
        let before = tree(&prefix);
        let error = store
            .upgrade(vec![(
                alpha.clone(),
                prepare(&store, "alpha", "2.0", &[("tool", true)]),
            )])
            .expect_err("foreign bin collision must fail");
        assert!(format!("{error:#}").contains("already exists"), "{error:#}");
        assert_eq!(tree(&prefix), before);
        assert_eq!(fs::read(&foreign).expect("foreign"), b"keep");

        let error = store
            .upgrade(vec![
                (
                    alpha.clone(),
                    prepare(&store, "alpha", "2.0", &[("shared", true)]),
                ),
                (
                    beta.clone(),
                    prepare(&store, "beta", "2.0", &[("shared", true)]),
                ),
            ])
            .expect_err("batch bin collision must fail");
        assert!(
            format!("{error:#}").contains("more than one package"),
            "{error:#}"
        );
        assert_eq!(tree(&prefix), before);

        let error = store
            .upgrade(vec![
                (
                    alpha.clone(),
                    prepare(&store, "alpha", "2.0", &[("alpha", true)]),
                ),
                (
                    alpha.clone(),
                    prepare(&store, "alpha", "3.0", &[("alpha", true)]),
                ),
            ])
            .expect_err("duplicate package name must fail");
        assert!(format!("{error:#}").contains("more than once"), "{error:#}");
        assert_eq!(tree(&prefix), before);

        let alternate = prefix.join(CELLAR).join("alpha").join("0.9");
        fs::create_dir(&alternate).expect("alternate version");
        let damaged = tree(&prefix);
        let error = store
            .upgrade(vec![(
                alpha,
                prepare(&store, "alpha", "2.0", &[("alpha", true)]),
            )])
            .expect_err("alternate version must fail");
        assert!(
            format!("{error:#}").contains("not the installed version"),
            "{error:#}"
        );
        assert_eq!(tree(&prefix), damaged);
        assert!(alternate.is_dir());
    }

    #[test]
    fn upgrade_rolls_back_new_paths_and_restores_the_old_package() {
        let (_temp, store) = new_store();
        let alpha = install(
            &store,
            "alpha",
            "1.0",
            &[("alpha", true), ("alpha-tool", true)],
        );
        let beta = install(&store, "beta", "1.0", &[("beta", true)]);
        let prefix = store.prefix().to_path_buf();
        fs::write(prefix.join("keep.txt"), b"keep").expect("foreign file");
        let before = tree(&prefix);

        let first = prepare(
            &store,
            "alpha",
            "2.0",
            &[("alpha", true), ("alpha-next", true)],
        );
        let second = prepare(&store, "beta", "2.0", &[("beta", true)]);
        let error = store
            .upgrade_inner(
                vec![(alpha.clone(), first), (beta.clone(), second)],
                Some(1),
            )
            .expect_err("injected upgrade failure");
        assert!(format!("{error:#}").contains("injected"), "{error:#}");

        assert_eq!(tree(&prefix), before);
        assert_eq!(
            read_only(&store).installed().expect("installed"),
            vec![alpha, beta]
        );
        assert!(
            fs::read_dir(prefix.join(META).join(STAGES))
                .expect("stages")
                .next()
                .is_none(),
            "a complete rollback drops the quarantine"
        );
        assert!(!prefix.join(META).join(STATE).join(LOCK_FILE).exists());
    }

    #[test]
    fn receipts_written_without_dependencies_still_load_and_are_satisfiable() {
        let (_temp, store) = new_store();
        let legacy = serde_json::json!({
            "name": "legacy",
            "version": "1.0",
            "sha256": digest(),
            "requested": true,
        })
        .to_string();
        fs::write(
            receipts_dir(store.prefix()).join("legacy.json"),
            legacy.as_bytes(),
        )
        .expect("write a pre-dependency receipt");

        let installed = store.installed().expect("installed");
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "legacy");
        assert!(installed[0].dependencies.is_empty());

        let dependent = prepare_with(
            &store,
            "dependent",
            "1.0",
            &[("dependent", true)],
            &[dependency(&installed[0])],
        );
        store
            .commit(vec![dependent])
            .expect("a dependent may pin a legacy receipt");
        assert_eq!(store.installed().expect("installed").len(), 2);
    }

    #[test]
    fn commit_installs_a_dependency_graph_transactionally() {
        let (_temp, store) = new_store();
        let libfoo = prepare(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let libfoo_receipt = libfoo.receipt.clone();
        let app = prepare_with(
            &store,
            "app",
            "1.0",
            &[("app", true)],
            &[dependency(&libfoo_receipt)],
        );
        store
            .commit(vec![libfoo, app])
            .expect("install a coherent dependency graph");

        let installed = store.installed().expect("installed");
        assert_eq!(installed.len(), 2);
        assert_eq!(installed[0].name, "app");
        assert_eq!(installed[0].dependencies, vec![dependency(&installed[1])]);

        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        // A version that is not installed.
        let mut missing_version = prepare(&store, "tool", "1.0", &[("tool", true)]);
        missing_version.receipt.dependencies = vec![Dependency {
            name: "libfoo".into(),
            version: "2.0".into(),
            sha256: digest(),
        }];
        let error = store
            .commit(vec![missing_version])
            .expect_err("a missing dependency version must fail");
        let message = format!("{error:#}");
        assert!(message.contains("'tool'"), "{message}");
        assert!(message.contains("libfoo 2.0"), "{message}");
        assert_eq!(tree(&prefix), before);

        // The recorded version with a different digest is a different pin.
        let mut wrong_digest = prepare(&store, "other", "1.0", &[("other", true)]);
        wrong_digest.receipt.dependencies = vec![Dependency {
            name: "libfoo".into(),
            version: "1.0".into(),
            sha256: "b".repeat(64),
        }];
        let error = store
            .commit(vec![wrong_digest])
            .expect_err("a mismatched dependency digest must fail");
        let message = format!("{error:#}");
        assert!(message.contains("'other'"), "{message}");
        assert!(message.contains("libfoo 1.0"), "{message}");
        assert_eq!(tree(&prefix), before);

        // One dangling edge rejects the whole batch before any package applies.
        let good = prepare(&store, "good", "1.0", &[("good", true)]);
        let mut bad = prepare(&store, "bad", "1.0", &[("bad", true)]);
        bad.receipt.dependencies = vec![Dependency {
            name: "absent".into(),
            version: "1.0".into(),
            sha256: digest(),
        }];
        let error = store
            .commit(vec![good, bad])
            .expect_err("a dangling edge must fail the batch");
        assert!(format!("{error:#}").contains("'bad'"), "{error:#}");
        assert_eq!(tree(&prefix), before);
        assert_eq!(store.installed().expect("installed").len(), 2);
        assert!(!prefix.join(CELLAR).join("good").exists());
        assert!(!prefix.join(CELLAR).join("bad").exists());
    }

    #[test]
    fn commit_with_promotions_promotes_and_installs_in_one_transaction() {
        let (_temp, store) = new_store();
        let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
        let original_bytes = fs::read(&receipt_path).expect("original receipt");
        let link = store.prefix().join(BIN).join("libfoo");
        let link_target = fs::read_link(&link).expect("existing link");

        let app = prepare_with(
            &store,
            "app",
            "1.0",
            &[("app", true)],
            &[dependency(&libfoo)],
        );
        let app_receipt = app.receipt.clone();
        store
            .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
            .expect("install app and promote libfoo");

        let installed = store.installed().expect("installed");
        let promoted = installed
            .iter()
            .find(|receipt| receipt.name == "libfoo")
            .expect("libfoo stays installed");
        let mut expected = libfoo.clone();
        expected.requested = true;
        assert_eq!(*promoted, expected, "only the requested flag changes");
        assert!(
            installed.contains(&app_receipt),
            "the new package is installed"
        );
        assert_eq!(
            fs::read(&receipt_path).expect("promoted receipt"),
            encode_receipt(&expected).expect("encode promoted receipt")
        );
        assert_ne!(
            fs::read(&receipt_path).expect("promoted receipt"),
            original_bytes
        );
        assert_eq!(
            fs::read_link(&link).expect("link survives promotion"),
            link_target
        );
        assert!(
            store
                .prefix()
                .join(CELLAR)
                .join("libfoo")
                .join("1.0")
                .is_dir(),
            "the dependency keg is untouched"
        );
        assert!(
            !store
                .prefix()
                .join(META)
                .join(STATE)
                .join(LOCK_FILE)
                .exists()
        );
    }

    #[test]
    fn commit_with_promotions_promotes_without_new_packages() {
        let (_temp, store) = new_store();
        let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);

        store
            .commit_with_promotions(Vec::new(), std::slice::from_ref(&libfoo))
            .expect("promote an installed dependency");

        let installed = store.installed().expect("installed");
        assert_eq!(installed.len(), 1);
        assert!(installed[0].requested);
        assert_eq!(installed[0].name, "libfoo");
    }

    #[test]
    fn commit_with_promotions_without_expected_delegates_to_commit() {
        let (_temp, store) = new_store();
        let prepared = prepare(&store, "hello", "1.0", &[("hello", true)]);
        let expected = prepared.receipt.clone();

        store
            .commit_with_promotions(vec![prepared], &[])
            .expect("plain commit");
        assert_eq!(store.installed().expect("installed"), vec![expected]);

        store
            .commit_with_promotions(Vec::new(), &[])
            .expect("empty commit is a no-op");

        let error = store
            .commit_with_promotions(vec![prepare(&store, "hello", "1.0", &[])], &[])
            .expect_err("the delegated commit still rejects an installed package");
        assert!(
            format!("{error:#}").contains("already installed"),
            "{error:#}"
        );
        assert_eq!(store.installed().expect("installed").len(), 1);
    }

    #[test]
    fn promotion_refuses_stale_missing_or_changed_receipts() {
        let (_temp, store) = new_store();
        let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let path = receipts_dir(store.prefix()).join("libfoo.json");

        let changes: [fn(&mut Receipt); 3] = [
            |receipt| receipt.requested = true,
            |receipt| receipt.sha256 = "b".repeat(64),
            |receipt| {
                receipt.dependencies = vec![Dependency {
                    name: "pin".into(),
                    version: "9.9".into(),
                    sha256: digest(),
                }]
            },
        ];
        for change in changes {
            let mut changed = libfoo.clone();
            change(&mut changed);
            let bytes = encode_receipt(&changed).expect("encode changed receipt");
            replace_receipt(&path, &bytes).expect("rewrite the receipt");

            let before = tree(store.prefix());
            let app = prepare(&store, "app", "1.0", &[("app", true)]);
            let error = store
                .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
                .expect_err("a changed receipt must be refused");
            let message = format!("{error:#}");
            assert!(message.contains("re-read"), "{message}");
            assert_eq!(tree(store.prefix()), before);
            assert_eq!(fs::read(&path).expect("changed receipt"), bytes);
            assert!(
                store
                    .installed()
                    .expect("installed")
                    .iter()
                    .all(|receipt| receipt.name != "app")
            );
        }

        fs::remove_file(&path).expect("remove the receipt");
        let before = tree(store.prefix());
        let app = prepare(&store, "app", "1.0", &[("app", true)]);
        let error = store
            .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
            .expect_err("a missing receipt must be refused");
        assert!(
            format!("{error:#}").contains("could not be re-read"),
            "{error:#}"
        );
        assert_eq!(tree(store.prefix()), before);
    }

    #[test]
    fn promotion_requires_an_owned_real_keg() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = new_store();
        let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let keg = store.prefix().join(CELLAR).join("libfoo").join("1.0");
        let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
        let original_bytes = fs::read(&receipt_path).expect("original receipt");
        let outside = tempdir().expect("outside directory");
        let parked = outside.path().join("keg");
        fs::rename(&keg, &parked).expect("park the keg");

        let app = prepare(&store, "app", "1.0", &[("app", true)]);
        let error = store
            .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
            .expect_err("a missing keg must be refused");
        assert!(format!("{error:#}").contains("keg"), "{error:#}");
        assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
        assert!(
            store
                .installed()
                .expect("installed")
                .iter()
                .all(|receipt| receipt.name != "app")
        );

        symlink(&parked, &keg).expect("replace the keg with a symlink");
        let app = prepare(&store, "app", "1.0", &[("app", true)]);
        let error = store
            .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
            .expect_err("a symlinked keg must be refused");
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
        assert!(!store.prefix().join(CELLAR).join("app").exists());
    }

    #[test]
    fn promotion_rejects_duplicates_overlap_and_already_requested_receipts() {
        let (_temp, store) = new_store();
        let alpha = install_unrequested(&store, "alpha", "1.0", &[("alpha", true)]);
        let requested = install(&store, "gamma", "1.0", &[("gamma", true)]);
        let before = tree(store.prefix());

        let error = store
            .commit_with_promotions(Vec::new(), &[alpha.clone(), alpha.clone()])
            .expect_err("a duplicate promotion must be refused");
        assert!(format!("{error:#}").contains("more than once"), "{error:#}");
        assert_eq!(tree(store.prefix()), before);

        let overlapping = prepare(&store, "alpha", "2.0", &[("alpha-next", true)]);
        let error = store
            .commit_with_promotions(vec![overlapping], std::slice::from_ref(&alpha))
            .expect_err("an install/promotion overlap must be refused");
        assert!(
            format!("{error:#}").contains("install and promote"),
            "{error:#}"
        );
        assert_eq!(tree(store.prefix()), before);

        let error = store
            .commit_with_promotions(Vec::new(), std::slice::from_ref(&requested))
            .expect_err("an already requested receipt must be refused");
        assert!(
            format!("{error:#}").contains("already requested"),
            "{error:#}"
        );
        assert_eq!(tree(store.prefix()), before);
    }

    #[test]
    fn promotion_rolls_back_new_paths_and_restores_promoted_receipt_bytes() {
        let (_temp, store) = new_store();
        let alpha = install_unrequested(&store, "alpha", "1.0", &[("alpha", true)]);
        let beta = install_unrequested(&store, "beta", "1.0", &[("beta", true)]);
        let alpha_path = receipts_dir(store.prefix()).join("alpha.json");
        let beta_path = receipts_dir(store.prefix()).join("beta.json");
        let alpha_bytes = fs::read(&alpha_path).expect("alpha receipt");
        let beta_bytes = fs::read(&beta_path).expect("beta receipt");
        let before = tree(store.prefix());

        let app = prepare(&store, "app", "1.0", &[("app", true)]);
        let error = store
            .commit_with_promotions_inner(vec![app], &[alpha, beta], None, Some(1))
            .expect_err("injected promotion failure");

        assert!(
            format!("{error:#}").contains("injected promotion failure"),
            "{error:#}"
        );
        assert_eq!(
            tree(store.prefix()),
            before,
            "new installs and the first promoted receipt are undone"
        );
        assert_eq!(fs::read(&alpha_path).expect("alpha receipt"), alpha_bytes);
        assert_eq!(fs::read(&beta_path).expect("beta receipt"), beta_bytes);
        let installed = store.installed().expect("installed");
        assert!(installed.iter().all(|receipt| receipt.name != "app"));
        assert!(installed.iter().all(|receipt| !receipt.requested));
    }

    #[test]
    fn promotion_keeps_receipts_when_a_new_install_fails() {
        let (_temp, store) = new_store();
        let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
        let original_bytes = fs::read(&receipt_path).expect("original receipt");
        let before = tree(store.prefix());

        let first = prepare(&store, "first", "1.0", &[("first", true)]);
        let second = prepare(&store, "second", "1.0", &[("second", true)]);
        let error = store
            .commit_with_promotions_inner(
                vec![first, second],
                std::slice::from_ref(&libfoo),
                Some(1),
                None,
            )
            .expect_err("injected install failure");

        assert!(
            format!("{error:#}").contains("injected commit failure"),
            "{error:#}"
        );
        assert_eq!(tree(store.prefix()), before);
        assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
        assert!(
            !store
                .installed()
                .expect("installed")
                .iter()
                .any(|receipt| receipt.name == "libfoo" && receipt.requested)
        );
    }

    #[test]
    fn promotion_validates_the_projected_graph_before_writing() {
        let (_temp, store) = new_store();
        let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
        let original_bytes = fs::read(&receipt_path).expect("original receipt");
        let before = tree(store.prefix());

        let mut app = prepare_with(
            &store,
            "app",
            "1.0",
            &[("app", true)],
            &[dependency(&libfoo)],
        );
        app.receipt.dependencies[0].sha256 = "b".repeat(64);
        let error = store
            .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
            .expect_err("a mismatched projected pin must fail");

        assert!(
            format!("{error:#}").contains("must match exactly"),
            "{error:#}"
        );
        assert_eq!(tree(store.prefix()), before);
        assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
        assert!(
            store
                .installed()
                .expect("installed")
                .iter()
                .all(|receipt| !receipt.requested)
        );
    }

    #[test]
    fn removal_blocks_unselected_dependents_and_allows_a_batch() {
        let (_temp, store) = new_store();
        let libfoo = install(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let app = install_with_dependency(&store, "app", "1.0", &libfoo, &[("app", true)]);
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        let error = read_only(&store)
            .remove(std::slice::from_ref(&libfoo))
            .expect_err("an installed dependent must block removal");
        let message = format!("{error:#}");
        assert!(message.contains("'app'"), "{message}");
        assert!(message.contains("libfoo 1.0"), "{message}");
        assert_eq!(tree(&prefix), before);
        assert_eq!(read_only(&store).installed().expect("installed").len(), 2);

        read_only(&store)
            .remove(&[libfoo, app])
            .expect("a batch that removes the dependent too");
        assert!(read_only(&store).installed().expect("installed").is_empty());
        assert!(!prefix.join(CELLAR).join("libfoo").exists());
        assert!(!prefix.join(CELLAR).join("app").exists());
    }

    #[test]
    fn upgrade_rejects_breaking_a_surviving_dependency_and_allows_coherent_batches() {
        let (_temp, store) = new_store();
        let libfoo = install(&store, "libfoo", "1.0", &[("libfoo", true)]);
        let app = install_with_dependency(&store, "app", "1.0", &libfoo, &[("app", true)]);
        let prefix = store.prefix().to_path_buf();
        let before = tree(&prefix);

        let breaking = prepare(&store, "libfoo", "2.0", &[("libfoo", true)]);
        let error = store
            .upgrade(vec![(libfoo.clone(), breaking)])
            .expect_err("a surviving dependent must block the upgrade");
        let message = format!("{error:#}");
        assert!(message.contains("'app'"), "{message}");
        assert!(message.contains("libfoo 1.0"), "{message}");
        assert!(message.contains("2.0"), "{message}");
        assert_eq!(tree(&prefix), before);

        // A dependent moved onto a version that is not installed also fails.
        let absent = prepare_with(
            &store,
            "app",
            "2.0",
            &[("app", true)],
            &[Dependency {
                name: "libfoo".into(),
                version: "9.0".into(),
                sha256: digest(),
            }],
        );
        let error = store
            .upgrade(vec![(app.clone(), absent)])
            .expect_err("an unresolvable new dependency must fail");
        assert!(format!("{error:#}").contains("'app'"), "{error:#}");
        assert_eq!(tree(&prefix), before);

        // Upgrading both sides in one call keeps the graph coherent.
        let libfoo_next = prepare(&store, "libfoo", "2.0", &[("libfoo", true)]);
        let libfoo_next_receipt = libfoo_next.receipt.clone();
        let app_next = prepare_with(
            &store,
            "app",
            "2.0",
            &[("app", true)],
            &[dependency(&libfoo_next_receipt)],
        );
        store
            .upgrade(vec![(libfoo, libfoo_next), (app, app_next)])
            .expect("a coherent batch upgrade");
        let installed = store.installed().expect("installed");
        assert_eq!(installed.len(), 2);
        assert_eq!(installed[0].name, "app");
        assert_eq!(installed[0].version, "2.0");
        assert_eq!(installed[0].dependencies[0].version, "2.0");
        assert_eq!(installed[1].version, "2.0");
    }

    #[test]
    fn dependency_validation_rejects_bad_pins_and_duplicate_graphs() {
        assert!(validate_dependencies(&[]).is_ok());

        let alpha = receipt("alpha", "1.0", true);
        let beta = receipt("beta", "1.0", true);
        assert!(validate_dependencies(std::slice::from_ref(&alpha)).is_ok());

        let mut pinned = alpha.clone();
        pinned.dependencies = vec![dependency(&beta)];
        assert!(validate_dependencies(&[pinned.clone(), beta.clone()]).is_ok());

        let error = validate_dependencies(std::slice::from_ref(&pinned))
            .expect_err("a missing dependency must fail");
        let message = format!("{error:#}");
        assert!(message.contains("'alpha'"), "{message}");
        assert!(message.contains("'beta 1.0'"), "{message}");

        let mut itself = alpha.clone();
        itself.dependencies = vec![dependency(&alpha)];
        let error = validate_dependencies(&[itself]).expect_err("a self dependency must fail");
        assert!(format!("{error:#}").contains("itself"), "{error:#}");

        let mut twice = alpha.clone();
        twice.dependencies = vec![dependency(&beta), dependency(&beta)];
        let error = validate_dependencies(&[twice, beta.clone()])
            .expect_err("a repeated dependency must fail");
        assert!(format!("{error:#}").contains("more than once"), "{error:#}");

        let error = validate_dependencies(&[alpha.clone(), alpha.clone()])
            .expect_err("a duplicate package must fail");
        assert!(
            format!("{error:#}").contains("projected receipt graph"),
            "{error:#}"
        );

        for bad_pin in [
            Dependency {
                name: "../evil".into(),
                version: "1.0".into(),
                sha256: digest(),
            },
            Dependency {
                name: "beta".into(),
                version: "../1".into(),
                sha256: digest(),
            },
            Dependency {
                name: "beta".into(),
                version: "1.0".into(),
                sha256: digest().to_uppercase(),
            },
            Dependency {
                name: "beta".into(),
                version: "1.0".into(),
                sha256: "a".repeat(63),
            },
        ] {
            let mut broken = alpha.clone();
            broken.dependencies = vec![bad_pin];
            assert!(validate_dependencies(&[broken]).is_err());
        }
    }

    #[test]
    fn filters_safe_bin_names() {
        for safe in [
            "hello",
            "python3.11",
            "libsigc++",
            "openssl@3",
            "_tool",
            "g++-13",
        ] {
            assert!(is_safe_bin_name(safe), "{safe} should be safe");
        }
        for unsafe_name in [
            "", ".", "..", ".hidden", "../evil", "a/b", "a b", "-rf", "a\\b",
        ] {
            assert!(
                !is_safe_bin_name(unsafe_name),
                "{unsafe_name} should be unsafe"
            );
        }
        assert!(is_safe_package_name("openssl@3"));
        assert!(!is_safe_package_name("OpenSSL"));
        assert!(is_safe_version("1.0.0-rc1"));
        assert!(!is_safe_version("1.0/2"));
        assert!(is_lower_hex_digest(&digest()));
        assert!(!is_lower_hex_digest(&digest().to_uppercase()));
    }
}
