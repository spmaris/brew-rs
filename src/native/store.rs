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
//! <prefix>/opt/<name>                   relative link into the active keg
//! <prefix>/bin/<member>                 relative links into the keg (omitted for keg-only)
//! <prefix>/sbin/<member>                administrative CLI links
//! <prefix>/share/man/man1..8/<page>     manual page links
//! <prefix>/etc/bash_completion.d/<file> bash completion links
//! <prefix>/share/zsh/site-functions/<f> zsh completion links
//! <prefix>/share/fish/vendor_completions.d/<f> fish completion links
//! <prefix>/lib/docker/cli-plugins/docker-<plugin> Docker CLI plugin links
//! ```
//!
//! A non-keg-only package also links direct regular files (or keg-contained
//! relative symlinks) from `share/man/man1` through `share/man/man8`,
//! `etc/bash_completion.d`, `share/zsh/site-functions`, and
//! `share/fish/vendor_completions.d`, plus executable `docker-*` members of
//! `lib/docker/cli-plugins`. Every linked directory uses the same transaction,
//! exact-target ownership, collision, and rollback rules as `bin`/`sbin`; a
//! member is linked only when it is a direct entry, never through a symlinked
//! parent directory, and removal recognizes a link only by its exact
//! version-specific target.
//!
//! Nothing here writes into a Homebrew prefix. A root is only usable when it
//! carries the marker, so an existing unrelated directory is never adopted by
//! accident, and known Homebrew prefixes are rejected after canonicalizing the
//! deepest existing ancestor (a symlinked path cannot smuggle a store into one).
//!
//! # Commit atomicity
//!
//! Each package is applied as: create `Cellar/<name>`, rename the staged root
//! to `Cellar/<name>/<version>`, create the managed links, then write the
//! receipt last. On failure everything created by this commit is rolled back in
//! reverse order. The lock serializes brewrs writers, so preflight collisions
//! are not expected to race with another commit.
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
//!
//! # Retained kegs
//!
//! When an upgrade replaces a package whose old exact `(name, version,
//! sha256)` is still pinned by a surviving receipt, the old keg is **retained**
//! instead of quarantined: the old keg directory stays in place under
//! `Cellar/<name>/<version>`, the old receipt moves to
//! `.brewrs/state/retained/<name>/<version>.json`, and the managed links
//! (`bin`, `sbin`, `opt`, manpages, completions, plugins) move to the new
//! version exactly as an unpinned upgrade would. A retained keg never keeps a
//! link and is never an install target: `opt` remains an active-version alias.
//! An upgrade whose old version is not pinned by any surviving receipt is
//! released exactly as before.
//!
//! Retained records preserve the old receipt bytes, so the retained identity
//! is version- and digest-qualified. A surviving receipt's pin is satisfied by
//! either the active receipt at that exact version and digest or a retained
//! record at that exact version and digest; a pin introduced by a *new*
//! receipt (an install or an upgrade replacement) is satisfied only by an
//! active receipt, never by a retained keg, so a new install can never
//! silently bind to a superseded keg. [`validate_dependencies_with_retained`]
//! exposes that pure check; [`validate_dependencies`] remains strict
//! active-only.
//!
//! Garbage collection is deferred: no transaction prunes a retained keg or
//! its record yet, even when the last pinning receipt is removed or upgraded.
//! A retained keg is inert (no links, no receipt), removal moves only the
//! active keg, and an emptied `Cellar/<name>` is pruned only when it holds
//! nothing at all, so retained kegs survive removals untouched. Fail-closed
//! guardrails instead refuse ambiguous or foreign state: an upgrade refuses a
//! `Cellar/<name>` entry that is neither the active keg nor a keg with a
//! matching retained record, a keg that is both active and retained, and a
//! retained directory or record that is a symlink or malformed; an install
//! refuses a retained-only keg whose version it would recreate and may only
//! create a version directory that is not already retained. Manual cleanup of
//! retained kegs remains a future milestone.

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
/// Root for admin CLI links.
const SBIN: &str = "sbin";
/// Root for active keg aliases (`opt/<name> -> ../Cellar/<name>/<version>`).
const OPT: &str = "opt";
/// Prefix-relative directory whose direct members are linked into the store,
/// together with the member rules for that directory.
///
/// `subdir` is both the keg-relative and prefix-relative path, so a link always
/// uses one `..` per path component. Every directory is optional: an older
/// brewrs prefix has only `bin`, `sbin`, and `opt`, and missing directories are
/// created on demand by the first package that provides a member.
#[derive(Clone, Copy)]
struct LinkDirectory {
    subdir: &'static str,
    /// Whether a member must be executable (CLI, admin CLI, Docker plugin).
    executable: bool,
    /// Optional required member-name prefix, as Docker uses for plugins.
    member_prefix: Option<&'static str>,
}

const LINK_DIRECTORIES: [LinkDirectory; 14] = [
    LinkDirectory {
        subdir: BIN,
        executable: true,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: SBIN,
        executable: true,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man1",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man2",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man3",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man4",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man5",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man6",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man7",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/man/man8",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "etc/bash_completion.d",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/zsh/site-functions",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "share/fish/vendor_completions.d",
        executable: false,
        member_prefix: None,
    },
    LinkDirectory {
        subdir: "lib/docker/cli-plugins",
        executable: true,
        member_prefix: Some("docker-"),
    },
];
/// Directory for brewrs-managed state.
const META: &str = ".brewrs";
const STATE: &str = "state";
const RECEIPTS: &str = "receipts";
const RETAINED: &str = "retained";
const STAGES: &str = "stages";
const LOCK_FILE: &str = "install.lock";
/// Longest accepted package name or version.
const MAX_NAME_LEN: usize = 128;
const MAX_VERSION_LEN: usize = 128;
/// Receipts are tiny; anything larger is corrupt or foreign.
const MAX_RECEIPT_BYTES: u64 = 16 * 1024;

/// Writer lock token/TempDir names stay unique within a process.
pub(super) static COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// The source used for installation and retained for future upgrades.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSource {
    #[default]
    Homebrew,
    Upstream,
}

impl PackageSource {
    /// Human-readable name used in user-facing messages.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Upstream => "upstream",
        }
    }
}

/// The durable record of one installed package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Receipt {
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

    /// Whether this store is bound to an existing owned prefix.
    pub(super) fn is_owned(&self) -> bool {
        self.state == RootState::Owned
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

    /// Read every retained keg record, sorted by name and version.
    ///
    /// A record is the exact receipt a past upgrade moved out of the active
    /// receipts directory when a surviving receipt kept pinning its keg. The
    /// directory layout is `.brewrs/state/retained/<name>/<version>.json`, so
    /// several versions of one package can be retained at once. A missing
    /// directory reads as empty, as does a binding to a prefix that is not an
    /// owned store.
    ///
    /// Reading fails closed on anything foreign or ambiguous: a symlinked
    /// retained or package directory, a package entry that is not a directory
    /// or not a valid package name, and a record that is not a regular,
    /// bounded JSON receipt naming exactly its directory and version stem.
    /// Unrelated entries that do not end in `.json` are ignored rather than
    /// misrepresented as pins.
    pub(super) fn retained(&self) -> Result<Vec<Receipt>> {
        if self.state != RootState::Owned {
            return Ok(Vec::new());
        }
        // A read-only caller (for example a reverse dependency query) must
        // not be redirected through a planted symlink: every ancestor of the
        // retained records has to be a real directory.
        for ancestor in [meta_dir(&self.prefix), state_dir(&self.prefix)] {
            match fs::symlink_metadata(&ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("refusing to read '{}': it is a symlink", ancestor.display())
                }
                Ok(metadata) if !metadata.is_dir() => bail!(
                    "refusing to read '{}': it is not a directory",
                    ancestor.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", ancestor.display()));
                }
            }
        }
        let directory = retained_dir(&self.prefix);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing to read '{}': the retained directory is a symlink",
                directory.display()
            ),
            Ok(metadata) if !metadata.is_dir() => bail!(
                "refusing to read '{}': the retained path is not a directory",
                directory.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }

        let mut records = Vec::new();
        for package in fs::read_dir(&directory)
            .with_context(|| format!("failed to read '{}'", directory.display()))?
        {
            let package = package
                .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
            let metadata = fs::symlink_metadata(package.path())
                .with_context(|| format!("failed to inspect '{}'", package.path().display()))?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to read '{}': the retained package directory is a symlink",
                    package.path().display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "refusing to read '{}': '{}' is not a retained package directory",
                    directory.display(),
                    package.path().display()
                );
            }
            let Some(name) = package.file_name().to_str().map(str::to_owned) else {
                bail!(
                    "refusing to read '{}': a retained package directory has a non-UTF-8 name",
                    directory.display()
                );
            };
            if !is_safe_package_name(&name) {
                bail!(
                    "refusing to read '{}': '{}' is not a valid retained package name",
                    directory.display(),
                    name
                );
            }
            let package_dir = package.path();
            for record in fs::read_dir(&package_dir)
                .with_context(|| format!("failed to read '{}'", package_dir.display()))?
            {
                let record = record.with_context(|| {
                    format!("failed to read an entry in '{}'", package_dir.display())
                })?;
                let Some(file_name) = record.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some(version) = file_name.strip_suffix(".json") else {
                    continue;
                };
                if !is_safe_version(version) {
                    bail!(
                        "refusing to read '{}': '{}' is not a valid retained version",
                        record.path().display(),
                        version
                    );
                }
                let receipt = read_receipt(&record.path())?;
                if receipt.name != name {
                    bail!(
                        "retained record '{}' names '{}', not '{}'",
                        record.path().display(),
                        receipt.name,
                        name
                    );
                }
                if receipt.version != version {
                    bail!(
                        "retained record '{}' records version '{}', not '{}'",
                        record.path().display(),
                        receipt.version,
                        version
                    );
                }
                records.push(receipt);
            }
        }
        records.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.version.cmp(&right.version))
        });
        Ok(records)
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
        let newcomers: Vec<Receipt> = pending
            .iter()
            .map(|package| package.receipt.clone())
            .collect();
        let retained = self.retained()?;
        validate_dependencies_with_retained(&projected, &newcomers, &retained)?;
        let mut active = projected.clone();
        active.extend(newcomers.iter().cloned());
        verify_retained_pin_kegs(&projected, &active, &retained, &cellar_dir(&self.prefix))?;
        projected.extend(newcomers);
        validate_conflicts(&projected)?;

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
    /// The projected receipt graph is validated first. A replacement that
    /// would break a surviving receipt's pin to the exact old version and
    /// digest retains the old keg and receipt instead of releasing them: the
    /// old keg stays in place, the old receipt moves to the retained records,
    /// and the managed links move to the replacement. A surviving pin that no
    /// retained record can satisfy still rejects the upgrade unless the
    /// dependent is upgraded in the same call.
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
        let opt = opt_dir(&self.prefix);
        let link_dirs = existing_link_directories(&self.prefix)?;
        let has_opt = check_optional_directory(&opt)?;
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

            // Scan the prefix's own managed link directories. Deriving names
            // from the keg would miss an owned link whose keg member was
            // deleted or made non-executable, and a reinstall would then
            // collide with it. Only an exact stored target identifies a brewrs
            // link; everything else is foreign and is preserved.
            let mut links = Vec::new();
            for root in LINK_DIRECTORIES {
                if let Some(directory) = link_dirs.get(root.subdir) {
                    links.extend(collect_owned_links(
                        directory,
                        &root,
                        &receipt.name,
                        &receipt.version,
                    )?);
                }
            }
            for link in &links {
                if !links_seen.insert(link.path.clone()) {
                    bail!(
                        "refusing to remove '{}': more than one selected package claims the link '{}'",
                        receipt.name,
                        link.path.display()
                    );
                }
            }
            if has_opt {
                let opt_path = opt.join(&receipt.name);
                let opt_target = opt_link_target(&receipt.name, &receipt.version);
                match fs::symlink_metadata(&opt_path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let actual = fs::read_link(&opt_path).with_context(|| {
                            format!("failed to read link '{}'", opt_path.display())
                        })?;
                        if actual.as_os_str() == opt_target.as_os_str() {
                            if !links_seen.insert(opt_path.clone()) {
                                bail!(
                                    "refusing to remove '{}': more than one selected package claims the link '{}'",
                                    receipt.name,
                                    opt_path.display()
                                );
                            }
                            links.push(OwnedLink {
                                path: opt_path,
                                target: opt_target,
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", opt_path.display())
                        });
                    }
                }
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
        // sees exactly the receipts those moves will delete. A remaining pin may
        // be satisfied by a retained keg, which removal never touches, and every
        // such pin must still own a real keg directory.
        let selected: HashSet<&str> = expected
            .iter()
            .map(|receipt| receipt.name.as_str())
            .collect();
        let remaining: Vec<Receipt> = self
            .installed()?
            .into_iter()
            .filter(|receipt| !selected.contains(receipt.name.as_str()))
            .collect();
        let retained = self.retained()?;
        validate_dependencies_with_retained(&remaining, &[], &retained)?;
        validate_retained_pin_closure(&retained, &remaining, &retained)?;
        let mut pin_owners = remaining.clone();
        pin_owners.extend(retained.iter().cloned());
        verify_retained_pin_kegs(&pin_owners, &remaining, &retained, &cellar)?;
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
                        AppliedChange::RetainedReceipt(_) => continue,
                        AppliedChange::Pruned(directory) => directory,
                    };
                    if let Some(parent) = source.parent()
                        && flushed.insert(parent.to_path_buf())
                    {
                        sync_directory(parent);
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
        let opt = opt_dir(&self.prefix);
        let link_dirs = existing_link_directories(&self.prefix)?;
        let has_opt = check_optional_directory(&opt)?;
        let retained = self.retained()?;
        let retained_by_name: HashMap<&str, HashMap<&str, &Receipt>> =
            retained.iter().fold(HashMap::new(), |mut by_name, record| {
                by_name
                    .entry(record.name.as_str())
                    .or_default()
                    .insert(record.version.as_str(), record);
                by_name
            });
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
            if old.version == receipt.version && old.version_scheme == receipt.version_scheme {
                bail!(
                    "refusing to upgrade '{}': version {} is already installed; the caller must skip it",
                    old.name,
                    old.version
                );
            }
            self.validate_stage(stage.path())?;
            let links = if receipt.keg_only {
                Vec::new()
            } else {
                staged_links(stage.path())?
            };

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

            // The package directory may hold the receipt's keg plus any kegs
            // with valid retained records. An entry with no retained record, a
            // version that is both active and retained, or a retained keg that
            // is not a real directory fails closed: only the active keg and
            // recorded retained kegs may survive this upgrade.
            if let Some(versions) = retained_by_name.get(old.name.as_str())
                && versions.contains_key(old.version.as_str())
            {
                bail!(
                    "refusing to upgrade '{}': the active version {} also has a retained record; remove the stale retained record manually",
                    old.name,
                    old.version
                );
            }
            for entry in fs::read_dir(&package)
                .with_context(|| format!("failed to read '{}'", package.display()))?
            {
                let entry = entry.with_context(|| {
                    format!("failed to read an entry in '{}'", package.display())
                })?;
                let file_name = entry.file_name();
                if file_name.as_os_str() == OsStr::new(&old.version) {
                    continue;
                }
                let Some(version) = file_name.to_str() else {
                    bail!(
                        "refusing to upgrade '{}': '{}' contains a foreign entry; remove it first",
                        old.name,
                        package.display()
                    );
                };
                let Some(record) = retained_by_name
                    .get(old.name.as_str())
                    .and_then(|versions| versions.get(version))
                else {
                    bail!(
                        "refusing to upgrade '{}': '{}' contains '{}', which is not the installed version {} and has no retained record; remove foreign entries first",
                        old.name,
                        package.display(),
                        file_name.to_string_lossy(),
                        old.version
                    );
                };
                if record.name != old.name || record.version != version {
                    bail!(
                        "refusing to upgrade '{}': the retained record for '{}' names '{} {}'; remove it manually",
                        old.name,
                        version,
                        record.name,
                        record.version
                    );
                }
                let metadata = fs::symlink_metadata(entry.path())
                    .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "refusing to upgrade '{}': retained keg '{}' is not a real directory",
                        old.name,
                        entry.path().display()
                    );
                }
            }

            let mut owned_links = Vec::new();
            for root in LINK_DIRECTORIES {
                if let Some(directory) = link_dirs.get(root.subdir) {
                    owned_links.extend(collect_owned_links(
                        directory,
                        &root,
                        &old.name,
                        &old.version,
                    )?);
                }
            }
            if has_opt {
                let opt_path = opt.join(&old.name);
                let opt_target = opt_link_target(&old.name, &old.version);
                match fs::symlink_metadata(&opt_path) {
                    Ok(metadata) => {
                        let owned = if metadata.file_type().is_symlink() {
                            let actual = fs::read_link(&opt_path).with_context(|| {
                                format!("failed to read link '{}'", opt_path.display())
                            })?;
                            actual.as_os_str() == opt_target.as_os_str()
                        } else {
                            false
                        };
                        if owned {
                            owned_links.push(OwnedLink {
                                path: opt_path,
                                target: opt_target,
                            });
                        } else {
                            bail!(
                                "refusing to upgrade '{}': '{}' already exists and is not a link owned by {} {}; refusing to overwrite it",
                                old.name,
                                opt_path.display(),
                                old.name,
                                old.version
                            );
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", opt_path.display())
                        });
                    }
                }
            }
            owned_links.sort_by(|left, right| left.path.cmp(&right.path));

            // A replacement may claim a managed link name only when this
            // package's own exact link currently occupies it; anything else is
            // foreign and is never overwritten. Cross-package claims fail in
            // `link_names`.
            for link in &links {
                if !link_names.insert(link.clone()) {
                    bail!(
                        "more than one package in this upgrade provides '{}/{}'",
                        link.subdir,
                        link.name
                    );
                }
                if let Some(directory) = link_dirs.get(link.subdir) {
                    let existing = directory.join(&link.name);
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
            }

            upgrades.push(Upgrade {
                old: Removal {
                    receipt_path,
                    package,
                    keg,
                    links: owned_links,
                },
                old_receipt: old,
                retain: false,
                pending: Pending {
                    receipt,
                    stage,
                    links,
                },
            });
        }

        // Decide retention before validation. An upgrade whose old receipt is
        // still pinned exactly (name, version, and digest) by a surviving
        // receipt retains the old keg and identity instead of releasing them;
        // the old keg stays in place and its receipt moves to the retained
        // records. A surviving pin that no active or retained record can
        // satisfy still rejects the upgrade, and a replacement pin must be
        // satisfied by an active receipt only, never by a retained keg.
        let survivors: Vec<Receipt> = self
            .installed()?
            .into_iter()
            .filter(|receipt| !names.contains(&receipt.name))
            .collect();
        let pinned_by = |target: &Receipt, owners: &[Receipt]| {
            owners.iter().any(|receipt| {
                receipt.dependencies.iter().any(|pin| {
                    pin.name == target.name
                        && pin.version == target.version
                        && pin.sha256 == target.sha256
                })
            })
        };
        // A pin owner is any surviving active receipt plus every retained
        // record already on disk: a retained keg's recorded identity keeps
        // the versions it pins, so it forces retention in later batches too.
        // (With garbage collection deferred, every retained record counts,
        // which is a safe superset of the still-reachable ones.)
        let mut owners = survivors.clone();
        owners.extend(retained.iter().cloned());
        let mut new_retained: Vec<Receipt> = Vec::new();
        for upgrade in &mut upgrades {
            if pinned_by(&upgrade.old_receipt, &owners) {
                upgrade.retain = true;
                new_retained.push(upgrade.old_receipt.clone());
            }
        }
        // Transitive closure: a retained old receipt's own pins must keep the
        // old receipts they match in this batch, so a chain of retained kegs
        // stays coherent instead of leaving one of them danging.
        loop {
            let kept: Vec<Receipt> = upgrades
                .iter()
                .filter(|upgrade| upgrade.retain)
                .map(|upgrade| upgrade.old_receipt.clone())
                .collect();
            let mut changed = false;
            for upgrade in &mut upgrades {
                if upgrade.retain {
                    continue;
                }
                if pinned_by(&upgrade.old_receipt, &kept) {
                    upgrade.retain = true;
                    new_retained.push(upgrade.old_receipt.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut all_retained = retained;
        all_retained.extend(new_retained);
        let replacements: Vec<Receipt> = upgrades
            .iter()
            .map(|upgrade| upgrade.pending.receipt.clone())
            .collect();
        validate_dependencies_with_retained(&survivors, &replacements, &all_retained)?;
        let mut final_receipts = survivors.clone();
        final_receipts.extend(replacements.iter().cloned());
        validate_retained_pin_closure(&all_retained, &final_receipts, &all_retained)?;
        verify_retained_pin_kegs(&owners, &final_receipts, &all_retained, &cellar)?;
        validate_conflicts(&final_receipts)?;
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
                if upgrade.retain {
                    // Retain the old keg in place. Only the owned links move
                    // aside (the replacement claims them), and the old receipt
                    // moves into the retained records so the exact old
                    // identity survives. The keg directory is never touched.
                    for link in &upgrade.old.links {
                        move_aside(
                            &RemovalStep {
                                source: link.path.clone(),
                                kind: StepKind::Link {
                                    target: link.target.clone(),
                                },
                            },
                            quarantine.path(),
                            index,
                            &mut moved,
                        )?;
                        index += 1;
                    }
                    retain_receipt(
                        &self.prefix,
                        &upgrade.old_receipt.name,
                        &upgrade.old_receipt.version,
                        &upgrade.old.receipt_path,
                        &mut moved,
                    )?;
                } else {
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
                    let mut parents: Vec<&Path> = Vec::new();
                    match change {
                        AppliedChange::Moved(moved) => {
                            if let Some(parent) = moved.source.parent() {
                                parents.push(parent);
                            }
                        }
                        AppliedChange::RetainedReceipt(moved) => {
                            if let Some(parent) = moved.source.parent() {
                                parents.push(parent);
                            }
                            if let Some(parent) = moved.parked.parent() {
                                parents.push(parent);
                            }
                        }
                        AppliedChange::Pruned(directory) => parents.push(directory),
                    }
                    for parent in parents {
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
            sbin_dir(prefix),
            opt_dir(prefix),
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
    pub(super) fn ensure_owned(&self) -> Result<()> {
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
    pub(super) fn verify_owned(&self) -> Result<()> {
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
        check_optional_directory(&sbin_dir(&self.prefix))?;
        check_optional_directory(&opt_dir(&self.prefix))?;
        check_optional_directory(&retained_dir(&self.prefix))?;
        check_optional_directory(&self.prefix.join("Caskroom"))?;
        check_optional_directory(&self.prefix.join("Applications"))?;
        check_optional_directory(&state_dir(&self.prefix).join("casks"))?;
        Ok(())
    }

    /// Check the whole batch against existing state before writing anything.
    fn prepare(&self, packages: Vec<Prepared>) -> Result<Vec<Pending>> {
        let receipts = receipts_dir(&self.prefix);
        let cellar = cellar_dir(&self.prefix);
        let opt = opt_dir(&self.prefix);
        let link_dirs = existing_link_directories(&self.prefix)?;
        let has_opt = check_optional_directory(&opt)?;
        let retained = self.retained()?;
        let retained_by_name: HashMap<&str, HashMap<&str, &Receipt>> =
            retained.iter().fold(HashMap::new(), |mut by_name, record| {
                by_name
                    .entry(record.name.as_str())
                    .or_default()
                    .insert(record.version.as_str(), record);
                by_name
            });
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
            let links = if receipt.keg_only {
                Vec::new()
            } else {
                staged_links(stage.path())?
            };

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
                Ok(_) => self.accept_existing_cellar(
                    &installed_dir,
                    &receipt,
                    retained_by_name.get(receipt.name.as_str()),
                )?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect '{}'", installed_dir.display())
                    });
                }
            }

            if has_opt {
                let existing_opt = opt.join(&receipt.name);
                match fs::symlink_metadata(&existing_opt) {
                    Ok(_) => bail!(
                        "refusing to install '{}': '{}' already exists; links are never overwritten",
                        receipt.name,
                        existing_opt.display()
                    ),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect '{}'", existing_opt.display())
                        });
                    }
                }
            }

            for link in &links {
                if !link_names.insert(link.clone()) {
                    bail!(
                        "more than one package in this commit provides '{}/{}'",
                        link.subdir,
                        link.name
                    );
                }
                if let Some(directory) = link_dirs.get(link.subdir) {
                    let existing = directory.join(&link.name);
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
            }

            pending.push(Pending {
                receipt,
                stage,
                links,
            });
        }

        // Full preflight of the graph this commit would leave behind. A
        // dependency may name a package from this same batch, but its exact
        // version and digest must match, and a pin recorded by a *new* receipt
        // is satisfied only by an active receipt, never by a retained keg.
        // Receipts already on disk may pin a retained keg, and every such pin
        // must still own a real keg directory.
        let survivors = self.installed()?;
        let newcomers: Vec<Receipt> = pending
            .iter()
            .map(|package| package.receipt.clone())
            .collect();
        validate_dependencies_with_retained(&survivors, &newcomers, &retained)?;
        let mut final_receipts = survivors.clone();
        final_receipts.extend(newcomers.iter().cloned());
        verify_retained_pin_kegs(&survivors, &final_receipts, &retained, &cellar)?;
        validate_conflicts(&final_receipts)?;
        Ok(pending)
    }

    /// A package directory without an active receipt may exist only when every
    /// entry is a retained keg backed by a valid retained record.
    ///
    /// Any other entry is foreign and fails closed: installing must never
    /// adopt an unreceipted keg, and recreating a version that is still
    /// retained is refused explicitly rather than rebinding it.
    fn accept_existing_cellar(
        &self,
        directory: &Path,
        receipt: &Receipt,
        retained: Option<&HashMap<&str, &Receipt>>,
    ) -> Result<()> {
        let metadata = fs::symlink_metadata(directory)
            .with_context(|| format!("failed to inspect '{}'", directory.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "refusing to install '{}': '{}' already exists without a receipt; remove it or use a clean prefix",
                receipt.name,
                directory.display()
            );
        }
        for entry in fs::read_dir(directory)
            .with_context(|| format!("failed to read '{}'", directory.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read an entry in '{}'", directory.display()))?;
            let Some(version) = entry.file_name().to_str().map(str::to_owned) else {
                bail!(
                    "refusing to install '{}': '{}' contains a foreign entry; remove it or use a clean prefix",
                    receipt.name,
                    directory.display()
                );
            };
            if version == receipt.version {
                bail!(
                    "refusing to install '{} {}': '{}' is retained by surviving receipts; install a different version or clean up the retained keg manually",
                    receipt.name,
                    receipt.version,
                    entry.path().display()
                );
            }
            let Some(record) = retained.and_then(|versions| versions.get(version.as_str())) else {
                bail!(
                    "refusing to install '{}': '{}' contains '{}', which has no retained receipt; remove foreign entries first",
                    receipt.name,
                    directory.display(),
                    version
                );
            };
            if record.name != receipt.name || record.version != version {
                bail!(
                    "refusing to install '{}': the retained record for '{}' names '{} {}'; remove it manually",
                    receipt.name,
                    version,
                    record.name,
                    record.version
                );
            }
            let metadata = fs::symlink_metadata(entry.path())
                .with_context(|| format!("failed to inspect '{}'", entry.path().display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "refusing to install '{}': retained keg '{}' is not a real directory",
                    receipt.name,
                    entry.path().display()
                );
            }
        }
        Ok(())
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
        let sbin = sbin_dir(&self.prefix);
        let opt = opt_dir(&self.prefix);
        let receipts = receipts_dir(&self.prefix);
        ensure_directory(&sbin, true)?;
        ensure_directory(&opt, true)?;

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
            // Preflight accepted a package directory that holds only retained
            // kegs; `create_dir` then fails with AlreadyExists and the
            // directory is reused. Anything else appearing here under the
            // writer lock fails closed.
            let mut cellar_created = !reuse_cellar;
            if cellar_created {
                match fs::create_dir(&package_cellar) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let metadata =
                            fs::symlink_metadata(&package_cellar).with_context(|| {
                                format!("failed to inspect '{}'", package_cellar.display())
                            })?;
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            bail!(
                                "refusing to install '{}': '{}' is no longer a real directory",
                                name,
                                package_cellar.display()
                            );
                        }
                        cellar_created = false;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "failed to create '{}' for {} {}",
                                package_cellar.display(),
                                name,
                                version
                            )
                        });
                    }
                }
            }
            let keg = package_cellar.join(&version);
            applied.push(Applied {
                cellar: package_cellar.clone(),
                cellar_created,
                keg: keg.clone(),
                keg_created: false,
                links: Vec::new(),
                link_dirs_created: Vec::new(),
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
            let opt_link_path = opt.join(&name);
            let opt_target = opt_link_target(&name, &version);
            create_symlink(&opt_target, &opt_link_path).with_context(|| {
                format!(
                    "failed to create the opt link '{}'",
                    opt_link_path.display()
                )
            })?;
            record.links.push(opt_link_path);
            for link in links {
                let Some(link_dir) = link_directory(
                    &self.prefix,
                    link.subdir,
                    Some(&mut record.link_dirs_created),
                )?
                else {
                    bail!(
                        "failed to create the link directory '{}'",
                        self.prefix.join(link.subdir).display()
                    );
                };
                let link_path = link_dir.join(&link.name);
                let target = keg_link_target(&name, &version, link.subdir, &link.name);
                create_symlink(&target, &link_path).with_context(|| {
                    format!(
                        "failed to create the managed link '{}'",
                        link_path.display()
                    )
                })?;
                record.links.push(link_path);
            }

            write_receipt(&receipts, &receipt)?;
        }
        Ok(())
    }
}

/// A symlink owned under one managed prefix link directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ManagedLink {
    subdir: &'static str,
    name: String,
}

/// A validated package awaiting application.
struct Pending {
    receipt: Receipt,
    stage: tempfile::TempDir,
    links: Vec<ManagedLink>,
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
    /// The exact old receipt this upgrade replaces; moved to the retained
    /// records when `retain` is true.
    old_receipt: Receipt,
    /// Whether the old keg and receipt are retained instead of released.
    retain: bool,
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
    link_dirs_created: Vec<PathBuf>,
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
    /// An active receipt moved into the retained records by a retaining
    /// upgrade; its keg stays in place.
    RetainedReceipt(MovedPath),
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

        for directory in record.link_dirs_created.iter().rev() {
            if let Err(error) = fs::remove_dir(directory)
                && error.kind() != io::ErrorKind::NotFound
            {
                issues.push(format!(
                    "failed to remove new link directory '{}': {error}",
                    directory.display()
                ));
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

/// Move a preflighted active receipt into the retained records, rechecking
/// that it is still a regular receipt and that no retained record already
/// exists for that exact version.
///
/// The destination derives from the receipt's own validated name and version,
/// so the move stays inside `.brewrs/state/retained/<name>/`. The keg stays
/// in place; only the receipt file moves.
fn retain_receipt(
    prefix: &Path,
    name: &str,
    version: &str,
    source: &Path,
    applied: &mut Vec<AppliedChange>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "refusing to retain '{}': it disappeared after preflight",
            source.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", source.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing to retain '{}': it is no longer a regular receipt",
            source.display()
        );
    }

    // The retained root may not exist yet (an initialized prefix from before
    // retention shipped never had one), so both levels are created lazily;
    // `ensure_directory` creates exactly one level and never a symlink.
    ensure_directory(&retained_dir(prefix), true)?;
    let package_dir = retained_dir(prefix).join(name);
    ensure_directory(&package_dir, true)?;
    let parked = package_dir.join(format!("{version}.json"));
    match fs::symlink_metadata(&parked) {
        Ok(_) => bail!(
            "refusing to retain '{} {}': '{}' already exists; remove the stale retained record manually",
            name,
            version,
            parked.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect '{}'", parked.display()));
        }
    }
    fs::rename(source, &parked).with_context(|| {
        format!(
            "failed to move the receipt of '{} {}' into the retained records",
            name, version
        )
    })?;
    sync_directory(&package_dir);
    sync_directory(&retained_dir(prefix));
    sync_directory(&state_dir(prefix));
    applied.push(AppliedChange::RetainedReceipt(MovedPath {
        source: source.to_path_buf(),
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
            AppliedChange::RetainedReceipt(moved) => {
                // The new receipt is removed before this runs, so the active
                // slot is expected to be free; anything else is never
                // overwritten and the retained record is left in place.
                match fs::symlink_metadata(&moved.source) {
                    Ok(_) => {
                        issues.push(format!(
                            "left the retained record '{}' in place: '{}' still exists",
                            moved.parked.display(),
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
                        "failed to restore the retained receipt '{}': {error}",
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

/// The exact relative target [`Store::commit`] stores for a managed member.
///
/// `subdir` is both keg- and prefix-relative, so it contributes one `..` per
/// path component: `bin/tool -> ../Cellar/.../bin/tool` and
/// `share/man/man1/tool.1 -> ../../../Cellar/.../share/man/man1/tool.1`.
fn keg_link_target(name: &str, version: &str, subdir: &str, member: &str) -> PathBuf {
    let mut target = PathBuf::new();
    for _ in subdir.split('/') {
        target.push("..");
    }
    target
        .join(CELLAR)
        .join(name)
        .join(version)
        .join(subdir)
        .join(member)
}

/// Scan one prefix link directory for symlinks whose exact relative target
/// matches `../.../Cellar/<name>/<version>/<subdir>/<member>`.
fn collect_owned_links(
    dir: &Path,
    root: &LinkDirectory,
    name: &str,
    version: &str,
) -> Result<Vec<OwnedLink>> {
    let mut links = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read '{}'", dir.display()))? {
        let entry =
            entry.with_context(|| format!("failed to read an entry in '{}'", dir.display()))?;
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
        if let Some(prefix) = root.member_prefix
            && !member.starts_with(prefix)
        {
            continue;
        }
        let path = entry.path();
        let target = keg_link_target(name, version, root.subdir, &member);
        let actual = fs::read_link(&path)
            .with_context(|| format!("failed to read link '{}'", path.display()))?;
        if actual.as_os_str() == target.as_os_str() {
            links.push(OwnedLink { path, target });
        }
    }
    Ok(links)
}

/// The exact relative target [`Store::commit`] stores for `opt/<name>`.
fn opt_link_target(name: &str, version: &str) -> PathBuf {
    Path::new("..").join(CELLAR).join(name).join(version)
}

/// Atomically publish a new file inside `directory` without overwriting `target`.
pub(super) fn write_new_file_atomic(directory: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
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
fn write_receipt(directory: &Path, receipt: &Receipt) -> Result<()> {
    let target = directory.join(format!("{}.json", receipt.name));
    let bytes = encode_receipt(receipt)?;
    write_new_file_atomic(directory, &target, &bytes)
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

/// Collect every linkable member below the staged keg's managed directories.
fn staged_links(stage: &Path) -> Result<Vec<ManagedLink>> {
    let mut links = Vec::new();
    for root in LINK_DIRECTORIES {
        for name in staged_members(stage, &root)? {
            links.push(ManagedLink {
                subdir: root.subdir,
                name,
            });
        }
    }
    Ok(links)
}

/// Resolve a managed directory below the stage without following symlinked
/// ancestors. `None` means the stage simply does not ship that directory.
fn staged_directory(stage: &Path, subdir: &str) -> Result<Option<PathBuf>> {
    let mut directory = stage.to_path_buf();
    for component in subdir.split('/') {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing stage '{}': '{subdir}' has a symlinked parent directory",
                stage.display()
            ),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }
    }
    Ok(Some(directory))
}

/// Direct regular files or keg-contained relative symlinks below one staged
/// managed directory.
fn staged_members(stage: &Path, root: &LinkDirectory) -> Result<Vec<String>> {
    let Some(dir) = staged_directory(stage, root.subdir)? else {
        return Ok(Vec::new());
    };

    let stage_root = stage
        .canonicalize()
        .with_context(|| format!("failed to resolve stage '{}'", stage.display()))?;
    let mut members = Vec::new();
    for entry in
        fs::read_dir(&dir).with_context(|| format!("failed to read '{}'", dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read an entry in '{}'", dir.display()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_safe_bin_name(&name) {
            continue;
        }
        if let Some(prefix) = root.member_prefix
            && !name.starts_with(prefix)
        {
            continue;
        }
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect '{}'", entry_path.display()))?;
        if file_type.is_file() {
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to inspect '{}'", entry_path.display()))?;
            if !root.executable || is_executable(&metadata) {
                members.push(name);
            }
        } else if file_type.is_symlink() {
            let raw_target = fs::read_link(&entry_path)
                .with_context(|| format!("failed to read link '{}'", entry_path.display()))?;
            if raw_target.is_absolute() {
                bail!(
                    "refusing stage '{}': '{}/{}' is an absolute symlink",
                    stage.display(),
                    root.subdir,
                    name
                );
            }
            let resolved = fs::canonicalize(&entry_path).with_context(|| {
                format!(
                    "refusing stage '{}': '{}/{}' is a broken symlink",
                    stage.display(),
                    root.subdir,
                    name
                )
            })?;
            if !resolved.starts_with(&stage_root) {
                bail!(
                    "refusing stage '{}': '{}/{}' escapes the staged keg",
                    stage.display(),
                    root.subdir,
                    name
                );
            }
            let target_meta = fs::symlink_metadata(&resolved)
                .with_context(|| format!("failed to inspect '{}'", resolved.display()))?;
            if target_meta.is_file() && (!root.executable || is_executable(&target_meta)) {
                members.push(name);
            }
        }
    }
    members.sort();
    Ok(members)
}

/// Resolve `<prefix>/<subdir>` while refusing any symlinked or non-directory
/// ancestor, and optionally create missing components.
///
/// `None` means an ancestor is missing and `create` is false. Existing
/// components are never followed when they are symlinks, so a foreign link
/// cannot redirect a managed link outside the prefix.
fn link_directory(
    prefix: &Path,
    subdir: &str,
    mut created: Option<&mut Vec<PathBuf>>,
) -> Result<Option<PathBuf>> {
    let mut directory = prefix.to_path_buf();
    for component in subdir.split('/') {
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                "refusing '{}': a brewrs path may not be a symlink",
                directory.display()
            ),
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => bail!(
                "refusing '{}': a brewrs path must be a directory",
                directory.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(created) = created.as_mut() else {
                    return Ok(None);
                };
                fs::create_dir(&directory)
                    .with_context(|| format!("failed to create '{}'", directory.display()))?;
                created.push(directory.clone());
                ensure_directory(&directory, false)?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect '{}'", directory.display()));
            }
        }
    }
    Ok(Some(directory))
}

/// Every managed link directory that already exists, keyed by prefix-relative
/// path. Missing directories are absent from the map and are created on demand
/// by a package that actually provides a member.
fn existing_link_directories(prefix: &Path) -> Result<HashMap<&'static str, PathBuf>> {
    let mut directories = HashMap::with_capacity(LINK_DIRECTORIES.len());
    for root in LINK_DIRECTORIES {
        if let Some(directory) = link_directory(prefix, root.subdir, None)? {
            directories.insert(root.subdir, directory);
        }
    }
    Ok(directories)
}

/// Create the relative CLI link, never replacing an existing path.
pub(super) fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
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

pub(super) fn is_safe_version(version: &str) -> bool {
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

pub(super) fn is_lower_hex_digest(value: &str) -> bool {
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

/// Validate a projected receipt graph as a whole.
///
/// This is pure: the caller supplies exactly the receipts the operation would
/// leave installed (commit adds, upgrade replaces, removal subtracts), and no
/// filesystem state is read. Every receipt's own fields are validated, package
/// names must be unique, and each recorded direct dependency must resolve to a
/// receipt with the same name, version, and digest. Errors name both the owner
/// and the affected dependency.
///
/// Production code uses [`validate_dependencies_with_retained`]; this strict
/// active-only form remains for tests of the pure graph rules.
#[cfg(test)]
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

/// Verify that every pin recorded by a retained receipt still resolves to an
/// active receipt or another retained record at its exact version and digest.
///
/// A retained keg is inert but its recorded identity describes what it was
/// built against; a removal or upgrade that would delete the active keg a
/// retained record pins is refused rather than silently stranding the
/// retained keg's recorded dependencies. This is pure: the caller supplies
/// the retained records, the active receipts the operation would leave
/// installed, and every retained record the operation would provide.
pub(super) fn validate_retained_pin_closure(
    retained: &[Receipt],
    active: &[Receipt],
    retained_set: &[Receipt],
) -> Result<()> {
    let active_by_name: HashMap<&str, &Receipt> = HashMap::from_iter(
        active
            .iter()
            .map(|receipt| (receipt.name.as_str(), receipt)),
    );
    let retained_by_name: HashMap<&str, HashSet<(&str, &str)>> =
        retained_set
            .iter()
            .fold(HashMap::new(), |mut by_name, record| {
                by_name
                    .entry(record.name.as_str())
                    .or_default()
                    .insert((record.version.as_str(), record.sha256.as_str()));
                by_name
            });
    for record in retained {
        for pin in &record.dependencies {
            let active_match = active_by_name
                .get(pin.name.as_str())
                .is_some_and(|actual| actual.version == pin.version && actual.sha256 == pin.sha256);
            if active_match {
                continue;
            }
            let retained_match = retained_by_name
                .get(pin.name.as_str())
                .is_some_and(|versions| {
                    versions.contains(&(pin.version.as_str(), pin.sha256.as_str()))
                });
            if !retained_match {
                bail!(
                    "the retained record for '{} {}' pins '{} {}' (sha256 {}), which this operation would strand; remove the retained record manually first",
                    record.name,
                    record.version,
                    pin.name,
                    pin.version,
                    pin.sha256
                );
            }
        }
    }
    Ok(())
}

/// Validate a projected receipt graph where pins recorded by *new* receipts
/// may only resolve to active receipts, while pins recorded by surviving
/// receipts may also resolve to retained records.
///
/// This is pure: the caller supplies the receipts the operation would leave
/// installed, split into the receipts that already existed (`receipts`) and
/// the receipts this operation writes (`new_receipts`), plus the retained
/// records on disk (or the records this operation would create). Every
/// receipt's own fields are validated, names must be unique across both
/// slices, and a retained record must be a valid receipt whose exact `(name,
/// version, sha256)` a pin can match.
///
/// A new receipt can never be satisfied by a retained keg: a retained record
/// is the inert identity of a superseded version, not an install target, so
/// an install or upgrade replacement that would pin one is refused explicitly
/// rather than silently binding to it.
pub(super) fn validate_dependencies_with_retained(
    receipts: &[Receipt],
    new_receipts: &[Receipt],
    retained: &[Receipt],
) -> Result<()> {
    let mut active: HashMap<&str, &Receipt> =
        HashMap::with_capacity(receipts.len() + new_receipts.len());
    for receipt in receipts.iter().chain(new_receipts) {
        validate_receipt(receipt)?;
        if active.insert(receipt.name.as_str(), receipt).is_some() {
            bail!(
                "package '{}' appears more than once in the projected receipt graph",
                receipt.name
            );
        }
    }
    let newcomers: HashSet<&str> = new_receipts
        .iter()
        .map(|receipt| receipt.name.as_str())
        .collect();
    let mut retained_by_name: HashMap<&str, HashSet<(&str, &str)>> = HashMap::new();
    for record in retained {
        validate_receipt(record)?;
        if !retained_by_name
            .entry(record.name.as_str())
            .or_default()
            .insert((record.version.as_str(), record.sha256.as_str()))
        {
            bail!(
                "retained record '{} {}' (sha256 {}) appears more than once",
                record.name,
                record.version,
                record.sha256
            );
        }
    }

    for receipt in receipts.iter().chain(new_receipts) {
        for dependency in &receipt.dependencies {
            let active_match = active.get(dependency.name.as_str()).filter(|actual| {
                actual.version == dependency.version && actual.sha256 == dependency.sha256
            });
            if active_match.is_some() {
                continue;
            }
            let retained_match =
                retained_by_name
                    .get(dependency.name.as_str())
                    .is_some_and(|versions| {
                        versions
                            .contains(&(dependency.version.as_str(), dependency.sha256.as_str()))
                    });
            if newcomers.contains(receipt.name.as_str()) {
                if retained_match {
                    bail!(
                        "'{}' depends on '{} {}' (sha256 {}), which only a retained keg provides; a new receipt must pin the installed version",
                        receipt.name,
                        dependency.name,
                        dependency.version,
                        dependency.sha256
                    );
                }
                bail!(
                    "'{}' depends on '{} {}' (sha256 {}), which the projected receipt graph does not contain; the dependency must be installed at that exact version and digest, or removed together with its dependent",
                    receipt.name,
                    dependency.name,
                    dependency.version,
                    dependency.sha256
                );
            }
            if !retained_match {
                bail!(
                    "'{}' depends on '{} {}' (sha256 {}), which neither the projected receipt graph nor the retained records contain; the dependency must match an installed or retained version exactly",
                    receipt.name,
                    dependency.name,
                    dependency.version,
                    dependency.sha256
                );
            }
        }
    }
    Ok(())
}

/// Verify that every pin only a retained record can satisfy still owns a real
/// keg directory.
///
/// A pin that matches no active receipt is checked against the retained
/// records, and each such pin must have a real (non-symlinked) keg at
/// `Cellar/<name>/<version>`; a missing or foreign keg means the record alone
/// cannot keep the dependent working, and the transaction fails closed
/// instead of trusting the record.
pub(super) fn verify_retained_pin_kegs(
    receipts: &[Receipt],
    active: &[Receipt],
    retained: &[Receipt],
    cellar: &Path,
) -> Result<()> {
    let active_by_name: HashMap<&str, &Receipt> = HashMap::from_iter(
        active
            .iter()
            .map(|receipt| (receipt.name.as_str(), receipt)),
    );
    let retained_by_name: HashMap<&str, HashSet<(&str, &str)>> =
        retained.iter().fold(HashMap::new(), |mut by_name, record| {
            by_name
                .entry(record.name.as_str())
                .or_default()
                .insert((record.version.as_str(), record.sha256.as_str()));
            by_name
        });
    for receipt in receipts {
        for pin in &receipt.dependencies {
            let active_match = active_by_name
                .get(pin.name.as_str())
                .is_some_and(|actual| actual.version == pin.version && actual.sha256 == pin.sha256);
            if active_match {
                continue;
            }
            let retained_match = retained_by_name
                .get(pin.name.as_str())
                .is_some_and(|versions| {
                    versions.contains(&(pin.version.as_str(), pin.sha256.as_str()))
                });
            if !retained_match {
                bail!(
                    "'{}' depends on '{} {}' (sha256 {}), which neither the projected receipt graph nor the retained records contain",
                    receipt.name,
                    pin.name,
                    pin.version,
                    pin.sha256
                );
            }
            let keg = cellar.join(&pin.name).join(&pin.version);
            match fs::symlink_metadata(&keg) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
                    "'{}' depends on '{} {}', but the retained keg '{}' is not a real directory",
                    receipt.name,
                    pin.name,
                    pin.version,
                    keg.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
                    "'{}' depends on '{} {}', but the retained keg '{}' is missing; restore the keg or remove the retained record manually",
                    receipt.name,
                    pin.name,
                    pin.version,
                    keg.display()
                ),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to inspect '{}'", keg.display()));
                }
            }
        }
    }
    Ok(())
}

/// Reject any projected receipt set where one package conflicts with another
/// present package. Checks both directions symmetrically so neither installing
/// a conflicting formula nor keeping an older one with a declared conflict can
/// succeed.
pub(super) fn validate_conflicts(receipts: &[Receipt]) -> Result<()> {
    let installed: HashSet<&str> = receipts
        .iter()
        .map(|receipt| receipt.name.as_str())
        .collect();
    for receipt in receipts {
        for conflict in &receipt.conflicts_with {
            if installed.contains(conflict.as_str()) {
                bail!(
                    "'{}' conflicts with '{}'; remove '{}' before installing '{}'",
                    receipt.name,
                    conflict,
                    conflict,
                    receipt.name
                );
            }
        }
    }
    Ok(())
}

/// Serialize writers for one prefix with an exclusive lock file.
pub(super) struct InstallLock {
    path: PathBuf,
    token: String,
}

impl InstallLock {
    pub(super) fn acquire(prefix: &Path) -> Result<Self> {
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
        if let Ok(contents) = fs::read(&self.path)
            && contents == self.token.as_bytes()
        {
            let _ = fs::remove_file(&self.path);
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
pub(super) fn ensure_directory(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': a brewrs path may not be a symlink",
            path.display()
        ),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => bail!(
            "refusing '{}': a brewrs path must be a directory",
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
pub(super) fn check_optional_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing '{}': a brewrs path may not be a symlink",
            path.display()
        ),
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => bail!(
            "refusing '{}': a brewrs path must be a directory",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Reject the filesystem root and any known Homebrew prefix, resolving
/// existing ancestors so a symlink cannot point a store into Homebrew.
pub(super) fn reject_protected_prefix(prefix: &Path) -> Result<()> {
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
pub(super) fn canonical_candidate(path: &Path) -> Result<PathBuf> {
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

pub(super) fn sync_directory(directory: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(directory)
        && let Err(error) = handle.sync_all()
    {
        log::debug!("failed to flush '{}': {error}", directory.display());
    }
    #[cfg(not(unix))]
    let _ = directory;
}

pub(super) fn monotonic_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn marker_path(prefix: &Path) -> PathBuf {
    prefix.join(MARKER_FILE)
}

pub(super) fn meta_dir(prefix: &Path) -> PathBuf {
    prefix.join(META)
}

pub(super) fn state_dir(prefix: &Path) -> PathBuf {
    meta_dir(prefix).join(STATE)
}

fn receipts_dir(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(RECEIPTS)
}

/// Records of kegs retained because a surviving receipt still pins their exact
/// version and digest: `<retained>/<name>/<version>.json`.
fn retained_dir(prefix: &Path) -> PathBuf {
    state_dir(prefix).join(RETAINED)
}

pub(super) fn stages_dir(prefix: &Path) -> PathBuf {
    meta_dir(prefix).join(STAGES)
}

fn cellar_dir(prefix: &Path) -> PathBuf {
    prefix.join(CELLAR)
}

pub(super) fn bin_dir(prefix: &Path) -> PathBuf {
    prefix.join(BIN)
}

fn sbin_dir(prefix: &Path) -> PathBuf {
    prefix.join(SBIN)
}

fn opt_dir(prefix: &Path) -> PathBuf {
    prefix.join(OPT)
}

#[cfg(test)]
mod tests;
