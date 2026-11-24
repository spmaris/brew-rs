//! Typed failures of the store layer.
//!
//! Every `Display` text here is the exact message the store has always
//! produced. Where a message used to be built from context layers ("X: Y"),
//! the outer layer is the variant's `Display` and the inner one is its
//! `source`, so chain printing yields the same combined text.

use std::io;
use std::path::{Path, PathBuf};

use super::MARKER_FILE;
use super::receipt::Dependency;

/// What a transaction was doing when a path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Install,
    Promote,
    Remove,
    Upgrade,
    Retain,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Promote => "promote",
            Self::Remove => "remove",
            Self::Upgrade => "upgrade",
            Self::Retain => "retain",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Install => "commit",
            Self::Promote => "promotion",
            Self::Remove => "removal",
            Self::Upgrade => "upgrade",
            Self::Retain => "retention",
        }
    }
}

/// How a package directory or keg failed its preflight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirFault {
    Symlink,
    NotDirectory,
    Missing,
}

fn package_directory_message(
    action: Action,
    name: &str,
    version: &str,
    path: &Path,
    fault: DirFault,
) -> String {
    let path = path.display();
    let verb = action.verb();
    match (fault, action) {
        (DirFault::Symlink, Action::Promote) => {
            format!("refusing to promote '{name} {version}': '{path}' is a symlink")
        }
        (DirFault::NotDirectory, Action::Promote) => {
            format!("refusing to promote '{name} {version}': '{path}' is not a directory")
        }
        (DirFault::Symlink, _) => format!("refusing to {verb} '{name}': '{path}' is a symlink"),
        (DirFault::NotDirectory, _) => {
            format!("refusing to {verb} '{name}': '{path}' is not a directory")
        }
        (DirFault::Missing, _) => {
            format!(
                "refusing to {verb} '{name} {version}': '{path}' is missing; remove it manually"
            )
        }
    }
}

fn keg_message(action: Action, name: &str, version: &str, path: &Path, fault: DirFault) -> String {
    let path = path.display();
    let verb = action.verb();
    match (fault, action) {
        (DirFault::Symlink, Action::Promote) => {
            format!("refusing to promote '{name} {version}': its keg '{path}' is a symlink")
        }
        (DirFault::NotDirectory, Action::Promote) => {
            format!("refusing to promote '{name} {version}': its keg '{path}' is not a directory")
        }
        (DirFault::Symlink, _) => {
            format!("refusing to {verb} '{name} {version}': '{path}' is a symlink")
        }
        (DirFault::NotDirectory, _) => {
            format!("refusing to {verb} '{name} {version}': '{path}' is not a directory")
        }
        (DirFault::Missing, _) => format!(
            "refusing to {verb} '{name} {version}': its keg '{path}' is missing; remove it manually"
        ),
    }
}

/// Failure of a store operation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    // ---- filesystem and serialisation ----
    /// A filesystem call failed; `action` is the full leading phrase, such as
    /// `failed to inspect`, and the path follows it in quotes.
    #[error("{action} '{}'", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create '{}' for {name} {version}", path.display())]
    CreateCellar {
        path: PathBuf,
        name: String,
        version: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to move the staged {name} {version} into '{}'", path.display())]
    MoveStage {
        name: String,
        version: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to move '{}' into the removal quarantine", path.display())]
    QuarantineMove {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to move the receipt of '{name} {version}' into the retained records")]
    RetainMove {
        name: String,
        version: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode the receipt for '{name}'")]
    ReceiptEncode {
        name: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to decode the receipt '{}'", path.display())]
    ReceiptDecode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not resolve '{}': {error}", path.display())]
    ResolveFailed { path: PathBuf, error: io::Error },
    #[error("could not resolve '{}'", path.display())]
    NoParent { path: PathBuf },
    #[error("could not resolve '{}': too many symlinks", path.display())]
    TooManySymlinks { path: PathBuf },
    #[error("internal error: applied record missing after push")]
    AppliedRecordMissing,
    #[error("injected {what} failure before {unit} {index}")]
    InjectedFailure {
        what: &'static str,
        unit: &'static str,
        index: usize,
    },

    // ---- lock ----
    #[error(
        "another brewrs install holds the lock '{}' ({owner}); if no brewrs process is running, the lock is stale: remove '{}' and retry",
        path.display(),
        path.display()
    )]
    LockHeld { path: PathBuf, owner: String },

    // ---- prefix ownership, protected prefixes, markers ----
    #[error("refusing an empty brewrs prefix")]
    EmptyPrefix,
    #[error("refusing relative brewrs prefix '{}'; use an absolute path", prefix.display())]
    RelativePrefix { prefix: PathBuf },
    #[error("refusing to modify '{}': it is not an initialized brewrs store", prefix.display())]
    NotInitialized { prefix: PathBuf },
    #[error("refusing to modify '{}': the store was opened read-only", prefix.display())]
    ReadOnly { prefix: PathBuf },
    #[error("refusing to use the filesystem root as a brewrs prefix")]
    RootPrefix,
    #[error(
        "refusing to use '{}' as a brewrs prefix: it is inside the Homebrew prefix '{}'; choose a dedicated directory such as ~/.brewrs",
        prefix.display(),
        homebrew.display()
    )]
    HomebrewPrefix { prefix: PathBuf, homebrew: PathBuf },
    #[error("refusing '{}': a brewrs prefix may not be a symlink", prefix.display())]
    PrefixSymlink { prefix: PathBuf },
    #[error("refusing '{}': a brewrs prefix must be a directory", prefix.display())]
    PrefixNotDirectory { prefix: PathBuf },
    #[error("refusing '{}': the prefix is not a real directory", prefix.display())]
    PrefixNotRealDirectory { prefix: PathBuf },
    #[error("refusing '{}': the prefix is no longer a real directory", prefix.display())]
    PrefixChanged { prefix: PathBuf },
    #[error(
        "refusing to use '{}': it is not empty and has no {} marker; choose an empty directory or an existing brewrs prefix",
        prefix.display(),
        MARKER_FILE
    )]
    PrefixNotEmpty { prefix: PathBuf },
    #[error("refusing '{}': the {} marker is a symlink", prefix.display(), MARKER_FILE)]
    MarkerSymlink { prefix: PathBuf },
    #[error("refusing '{}': the {} marker is not a regular file", prefix.display(), MARKER_FILE)]
    MarkerNotRegular { prefix: PathBuf },
    #[error("refusing '{}': the {} marker is not a brewrs marker", prefix.display(), MARKER_FILE)]
    MarkerNotBrewrs { prefix: PathBuf },
    #[error("refusing '{}': the {} marker does not belong to brewrs", prefix.display(), MARKER_FILE)]
    MarkerForeign { prefix: PathBuf },
    #[error("refusing '{}': the marker disappeared while initializing it", prefix.display())]
    MarkerVanishedDuringInit { prefix: PathBuf },
    #[error("refusing '{}': the {} marker disappeared", prefix.display(), MARKER_FILE)]
    MarkerDisappeared { prefix: PathBuf },
    #[error("refusing '{}': a brewrs path may not be a symlink", path.display())]
    PathSymlink { path: PathBuf },
    #[error("refusing '{}': a brewrs path must be a directory", path.display())]
    PathNotDirectory { path: PathBuf },
    #[error("refusing '{}': the managed directory is missing", path.display())]
    ManagedDirectoryMissing { path: PathBuf },
    #[error("failed to create the link directory '{}'", path.display())]
    LinkDirectoryUnavailable { path: PathBuf },

    // ---- stages ----
    #[error("refusing stage '{}': it is not a real directory", stage.display())]
    StageNotDirectory { stage: PathBuf },
    #[error("refusing stage '{}': it is not owned by '{}'", stage.display(), stages.display())]
    StageNotOwned { stage: PathBuf, stages: PathBuf },
    #[error("refusing stage '{}': '{subdir}' has a symlinked parent directory", stage.display())]
    StageSymlinkedParent { stage: PathBuf, subdir: String },
    #[error("refusing stage '{}': '{subdir}/{member}' is an absolute symlink", stage.display())]
    StageAbsoluteSymlink {
        stage: PathBuf,
        subdir: &'static str,
        member: String,
    },
    #[error("refusing stage '{}': '{subdir}/{member}' is a broken symlink", stage.display())]
    StageBrokenSymlink {
        stage: PathBuf,
        subdir: &'static str,
        member: String,
        #[source]
        source: io::Error,
    },
    #[error("refusing stage '{}': '{subdir}/{member}' escapes the staged keg", stage.display())]
    StageEscapes {
        stage: PathBuf,
        subdir: &'static str,
        member: String,
    },

    // ---- receipts: invalid or corrupt ----
    #[error("refusing '{}': a receipt must be a regular file", path.display())]
    ReceiptNotRegular { path: PathBuf },
    #[error("refusing to replace '{}': a receipt must be a regular file", path.display())]
    ReplaceNotRegular { path: PathBuf },
    #[error("refusing to replace '{}': it has no parent directory", path.display())]
    ReceiptNoParent { path: PathBuf },
    #[error("refusing '{}': the receipt is too large", path.display())]
    ReceiptTooLarge { path: PathBuf },
    #[error("refusing to overwrite the existing receipt '{}'", path.display())]
    ReceiptExists { path: PathBuf },
    #[error("receipt '{}' names '{actual}', not '{expected}'", path.display())]
    ReceiptNameMismatch {
        path: PathBuf,
        actual: String,
        expected: String,
    },
    #[error("refusing to read '{}': the receipts directory is a symlink", path.display())]
    ReceiptsDirectorySymlink { path: PathBuf },
    #[error("refusing to read '{}': the receipts path is not a directory", path.display())]
    ReceiptsPathNotDirectory { path: PathBuf },
    #[error("refusing to read '{}': it is a symlink", path.display())]
    ReadSymlink { path: PathBuf },
    #[error("refusing to read '{}': it is not a directory", path.display())]
    ReadNotDirectory { path: PathBuf },
    #[error("refusing to read '{}': the retained directory is a symlink", path.display())]
    RetainedDirectorySymlink { path: PathBuf },
    #[error("refusing to read '{}': the retained path is not a directory", path.display())]
    RetainedPathNotDirectory { path: PathBuf },
    #[error("refusing to read '{}': the retained package directory is a symlink", path.display())]
    RetainedPackageSymlink { path: PathBuf },
    #[error(
        "refusing to read '{}': '{}' is not a retained package directory",
        directory.display(),
        path.display()
    )]
    NotRetainedPackageDirectory { directory: PathBuf, path: PathBuf },
    #[error(
        "refusing to read '{}': a retained package directory has a non-UTF-8 name",
        directory.display()
    )]
    RetainedPackageNonUtf8 { directory: PathBuf },
    #[error(
        "refusing to read '{}': '{name}' is not a valid retained package name",
        directory.display()
    )]
    InvalidRetainedPackageName { directory: PathBuf, name: String },
    #[error("refusing to read '{}': '{version}' is not a valid retained version", path.display())]
    InvalidRetainedVersion { path: PathBuf, version: String },
    #[error("retained record '{}' names '{actual}', not '{expected}'", path.display())]
    RetainedRecordNameMismatch {
        path: PathBuf,
        actual: String,
        expected: String,
    },
    #[error("retained record '{}' records version '{actual}', not '{expected}'", path.display())]
    RetainedRecordVersionMismatch {
        path: PathBuf,
        actual: String,
        expected: String,
    },
    #[error("retained record '{name} {version}' (sha256 {sha256}) appears more than once")]
    DuplicateRetainedRecord {
        name: String,
        version: String,
        sha256: String,
    },

    // ---- unsafe names, versions, digests ----
    #[error("refusing invalid package name '{name}'")]
    InvalidPackageName { name: String },
    #[error("refusing invalid version '{version}' for '{name}'")]
    InvalidVersion { name: String, version: String },
    #[error("refusing invalid sha256 for '{name}': expected 64 lowercase hexadecimal characters")]
    InvalidSha256 { name: String },
    #[error("refusing invalid dependency name '{name}' recorded by '{owner}'")]
    InvalidDependencyName { owner: String, name: String },
    #[error("refusing invalid dependency version '{version}' for '{name}' recorded by '{owner}'")]
    InvalidDependencyVersion {
        owner: String,
        name: String,
        version: String,
    },
    #[error(
        "refusing invalid sha256 for dependency '{name}' recorded by '{owner}': expected 64 lowercase hexadecimal characters"
    )]
    InvalidDependencySha256 { owner: String, name: String },
    #[error("refusing invalid conflict name '{conflict}' recorded by '{owner}'")]
    InvalidConflictName { owner: String, conflict: String },

    // ---- dependency and pin validation ----
    #[error("refusing '{name}': it lists itself as a dependency")]
    SelfDependency { name: String },
    #[error("refusing '{name}': dependency '{dependency}' is listed more than once")]
    DuplicateDependency { name: String, dependency: String },
    #[error("refusing '{name}': it lists itself as a conflict")]
    SelfConflict { name: String },
    #[error("refusing '{name}': conflict '{conflict}' is listed more than once")]
    DuplicateConflict { name: String, conflict: String },
    #[error("package '{name}' appears more than once in the projected receipt graph")]
    DuplicateInGraph { name: String },
    #[error(
        "'{owner}' depends on '{} {}' (sha256 {}), which the projected receipt graph does not contain; the dependency must be installed at that exact version and digest, or removed together with its dependent",
        pin.name, pin.version, pin.sha256
    )]
    DependencyMissing { owner: String, pin: Box<Dependency> },
    #[cfg(test)]
    #[error(
        "'{owner}' depends on '{} {}' (sha256 {}), but the projected receipt graph records '{}' as {actual_version} (sha256 {actual_sha256}); the dependency must match exactly",
        pin.name, pin.version, pin.sha256, pin.name
    )]
    DependencyMismatch {
        owner: String,
        pin: Box<Dependency>,
        actual_version: String,
        actual_sha256: String,
    },
    #[error(
        "'{owner}' depends on '{} {}' (sha256 {}), which only a retained keg provides; a new receipt must pin the installed version",
        pin.name, pin.version, pin.sha256
    )]
    DependencyOnlyRetained { owner: String, pin: Box<Dependency> },
    #[error(
        "'{owner}' depends on '{} {}' (sha256 {}), which neither the projected receipt graph nor the retained records contain; the dependency must match an installed or retained version exactly",
        pin.name, pin.version, pin.sha256
    )]
    DependencyNotProvided { owner: String, pin: Box<Dependency> },
    #[error(
        "'{owner}' depends on '{} {}' (sha256 {}), which neither the projected receipt graph nor the retained records contain",
        pin.name, pin.version, pin.sha256
    )]
    DependencyUnresolved { owner: String, pin: Box<Dependency> },
    #[error(
        "the retained record for '{record_name} {record_version}' pins '{} {}' (sha256 {}), which this operation would strand; remove the retained record manually first",
        pin.name, pin.version, pin.sha256
    )]
    RetainedPinStranded {
        record_name: String,
        record_version: String,
        pin: Box<Dependency>,
    },
    #[error(
        "'{owner}' depends on '{name} {version}', but the retained keg '{}' is not a real directory",
        keg.display()
    )]
    RetainedPinKegNotDirectory {
        owner: String,
        name: String,
        version: String,
        keg: PathBuf,
    },
    #[error(
        "'{owner}' depends on '{name} {version}', but the retained keg '{}' is missing; restore the keg or remove the retained record manually",
        keg.display()
    )]
    RetainedPinKegMissing {
        owner: String,
        name: String,
        version: String,
        keg: PathBuf,
    },

    // ---- conflicts: another package owns the link, file, or name ----
    #[error("'{name}' conflicts with '{conflict}'; remove '{conflict}' before installing '{name}'")]
    PackageConflict { name: String, conflict: String },
    #[error(
        "refusing to install '{name}': '{}' already exists; links are never overwritten",
        path.display()
    )]
    LinkExists { name: String, path: PathBuf },
    #[error("more than one package in this {} provides '{subdir}/{name}'", action.noun())]
    LinkClash {
        action: Action,
        subdir: &'static str,
        name: String,
    },
    #[error(
        "refusing to upgrade '{name}': '{}' already exists and is not a link owned by {name} {version}; refusing to overwrite it",
        path.display()
    )]
    LinkNotOwned {
        name: String,
        version: String,
        path: PathBuf,
    },
    #[error(
        "refusing to remove '{name}': more than one selected package claims the link '{}'",
        path.display()
    )]
    LinkClaimedTwice { name: String, path: PathBuf },

    // ---- already installed / not installed / stale observations ----
    #[error("package '{name}' appears more than once in one {}", action.noun())]
    DuplicatePackage { action: Action, name: String },
    #[error(
        "'{name} {version}' is already installed in '{}'; the caller must skip it",
        prefix.display()
    )]
    AlreadyInstalled {
        name: String,
        version: String,
        prefix: PathBuf,
    },
    #[error(
        "'{name}' is already installed at version {installed}; refusing to replace it with {requested}"
    )]
    AlreadyInstalledOtherVersion {
        name: String,
        installed: String,
        requested: String,
    },
    #[error(
        "refusing to upgrade '{name}': version {version} is already installed; the caller must skip it"
    )]
    UpgradeSameVersion { name: String, version: String },
    #[error("refusing to upgrade '{name}': the staged package is '{staged}'")]
    StagedNameMismatch { name: String, staged: String },
    #[error(
        "refusing to upgrade '{name} {version}': the replacement would change its requested status"
    )]
    RequestedChanged { name: String, version: String },
    #[error(
        "refusing to {} '{name} {version}': its receipt '{}' no longer exists",
        action.verb(),
        path.display()
    )]
    ReceiptGone {
        action: Action,
        name: String,
        version: String,
        path: PathBuf,
    },
    #[error(
        "refusing to {} '{name} {version}': the installed receipt now records {current_name} {current_version}; re-read the installed packages and try again",
        action.verb()
    )]
    ReceiptChanged {
        action: Action,
        name: String,
        version: String,
        current_name: String,
        current_version: String,
    },
    #[error("refusing to install and promote '{name}' in the same call")]
    InstallAndPromote { name: String },
    #[error("refusing to promote '{name} {version}': the observed receipt is already requested")]
    AlreadyRequested { name: String, version: String },
    #[error(
        "refusing to promote '{name} {version}': its receipt '{}' could not be re-read",
        path.display()
    )]
    PromotionReceiptUnreadable {
        name: String,
        version: String,
        path: PathBuf,
        #[source]
        source: Box<StoreError>,
    },
    #[error(
        "refusing to promote '{name} {version}': the installed receipt now records {current_name} {current_version} with requested={requested}; re-read the installed packages and try again"
    )]
    PromotionReceiptChanged {
        name: String,
        version: String,
        current_name: String,
        current_version: String,
        requested: bool,
    },
    #[error("failed to re-read '{}' before promoting '{name} {version}'", path.display())]
    PromotionReread {
        path: PathBuf,
        name: String,
        version: String,
        #[source]
        source: io::Error,
    },
    #[error("refusing to promote '{name} {version}': its receipt changed after preflight")]
    PromotionRaced { name: String, version: String },
    #[error("failed to mark '{name} {version}' as requested")]
    PromotionWrite {
        name: String,
        version: String,
        #[source]
        source: Box<StoreError>,
    },

    // ---- package directories, kegs, and retained kegs ----
    #[error("{}", package_directory_message(*action, name, version, path, *fault))]
    PackageDirectory {
        action: Action,
        name: String,
        version: String,
        path: PathBuf,
        fault: DirFault,
    },
    #[error("{}", keg_message(*action, name, version, path, *fault))]
    Keg {
        action: Action,
        name: String,
        version: String,
        path: PathBuf,
        fault: DirFault,
    },
    #[error(
        "refusing to install '{name}': '{}' already exists without a receipt; remove it or use a clean prefix",
        path.display()
    )]
    UnreceiptedPackageDirectory { name: String, path: PathBuf },
    #[error(
        "refusing to install '{name}': '{}' contains a foreign entry; remove it or use a clean prefix",
        path.display()
    )]
    ForeignCellarEntry { name: String, path: PathBuf },
    #[error(
        "refusing to install '{name} {version}': '{}' is retained by surviving receipts; install a different version or clean up the retained keg manually",
        path.display()
    )]
    VersionRetained {
        name: String,
        version: String,
        path: PathBuf,
    },
    #[error(
        "refusing to install '{name}': '{}' contains '{entry}', which has no retained receipt; remove foreign entries first",
        directory.display()
    )]
    UnrecordedCellarEntry {
        name: String,
        directory: PathBuf,
        entry: String,
    },
    #[error(
        "refusing to {} '{name}': the retained record for '{version}' names '{record_name} {record_version}'; remove it manually",
        action.verb()
    )]
    RetainedRecordMismatch {
        action: Action,
        name: String,
        version: String,
        record_name: String,
        record_version: String,
    },
    #[error(
        "refusing to {} '{name}': retained keg '{}' is not a real directory",
        action.verb(),
        path.display()
    )]
    RetainedKegNotDirectory {
        action: Action,
        name: String,
        path: PathBuf,
    },
    #[error(
        "refusing to upgrade '{name}': the active version {version} also has a retained record; remove the stale retained record manually"
    )]
    ActiveAlsoRetained { name: String, version: String },
    #[error(
        "refusing to upgrade '{name}': '{}' contains a foreign entry; remove it first",
        path.display()
    )]
    UpgradeForeignEntry { name: String, path: PathBuf },
    #[error(
        "refusing to upgrade '{name}': '{}' contains '{entry}', which is not the installed version {version} and has no retained record; remove foreign entries first",
        directory.display()
    )]
    UpgradeUnrecordedEntry {
        name: String,
        directory: PathBuf,
        entry: String,
        version: String,
    },
    #[error(
        "refusing to retain '{name} {version}': '{}' already exists; remove the stale retained record manually",
        path.display()
    )]
    RetainedRecordExists {
        name: String,
        version: String,
        path: PathBuf,
    },

    // ---- paths that changed after preflight ----
    #[error("refusing to install '{name}': '{}' is no longer a real directory", path.display())]
    CellarChanged { name: String, path: PathBuf },
    #[error("refusing to {} '{}': it disappeared after preflight", action.verb(), path.display())]
    Disappeared { action: Action, path: PathBuf },
    #[error("refusing to {} '{}': it is no longer a regular receipt", action.verb(), path.display())]
    NoLongerRegularReceipt { action: Action, path: PathBuf },
    #[error("refusing to remove '{}': it is no longer a real directory", path.display())]
    NoLongerRealDirectory { path: PathBuf },
    #[error("refusing to remove '{}': it is no longer a brewrs symlink", path.display())]
    NoLongerBrewrsSymlink { path: PathBuf },
    #[error(
        "refusing to remove '{}': the link target is now '{}'",
        path.display(),
        actual.display()
    )]
    LinkTargetChanged { path: PathBuf, actual: PathBuf },

    // ---- commit and rollback failures ----
    #[error("rollback was incomplete: {}", issues.join("; "))]
    CommitRollbackIncomplete {
        issues: Vec<String>,
        #[source]
        source: Box<StoreError>,
    },
    #[error(
        "rollback was incomplete: {}; the quarantined paths were kept in '{}'",
        issues.join("; "),
        kept.display()
    )]
    RemovalRollbackIncomplete {
        issues: Vec<String>,
        kept: PathBuf,
        #[source]
        source: Box<StoreError>,
    },
    #[error(
        "rollback was incomplete: {}; the quarantined old paths were kept in '{}'",
        issues.join("; "),
        kept.display()
    )]
    UpgradeRollbackIncomplete {
        issues: Vec<String>,
        kept: PathBuf,
        #[source]
        source: Box<StoreError>,
    },
}

impl StoreError {
    /// Adapter for `map_err` that attaches the failed action and its path to a
    /// filesystem error. The path is only copied when the error occurs.
    pub(crate) fn io<'a, P: AsRef<Path> + ?Sized>(
        action: &'static str,
        path: &'a P,
    ) -> impl FnOnce(io::Error) -> Self + 'a {
        move |source| Self::Io {
            action,
            path: path.as_ref().to_path_buf(),
            source,
        }
    }
}
