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

mod commit;
mod installed;
mod links;
mod lock;
mod paths;
mod prefix;
mod receipt;
mod remove;
mod stage;
mod upgrade;
mod validate;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

pub(crate) use self::links::create_symlink;
pub(crate) use self::lock::InstallLock;
pub(super) use self::paths::{bin_dir, meta_dir, monotonic_nonce};
pub(crate) use self::paths::{stages_dir, state_dir, sync_directory};
pub(super) use self::prefix::check_optional_directory;
pub(crate) use self::prefix::{canonical_candidate, ensure_directory, reject_protected_prefix};
pub(crate) use self::receipt::{Dependency, Prepared, Receipt, write_new_file_atomic};
#[cfg(test)]
pub(crate) use self::validate::validate_dependencies;
pub(crate) use self::validate::{
    validate_conflicts, validate_dependencies_with_retained, validate_retained_pin_closure,
};

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

/// Directory for brewrs-managed state.
const META: &str = ".brewrs";
const STATE: &str = "state";
const RECEIPTS: &str = "receipts";
const RETAINED: &str = "retained";
const STAGES: &str = "stages";
const LOCK_FILE: &str = "install.lock";
/// Longest accepted package name or version.
const MAX_NAME_LEN: usize = 128;
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
pub(crate) struct Store {
    prefix: PathBuf,
    state: RootState,
    writable: bool,
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

#[cfg(test)]
mod tests;
