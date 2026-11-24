//! Typed failures of the cask store.
//!
//! Every `Display` text is the exact message the cask store has always
//! produced; context layers are the variant's `Display` with the inner failure
//! as its `source`, so chain printing yields the same combined text.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use crate::domain::cask_plan::IdentifierError;
use crate::infra::store::StoreError;

/// Failure of a cask store operation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CaskStoreError {
    /// A failure of the shared formula store (markers, directories, locks,
    /// atomic file writes), passed through unchanged.
    #[error(transparent)]
    Store(#[from] StoreError),

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
    #[error("failed to encode cask receipt '{token}'")]
    ReceiptEncode {
        token: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to decode cask receipt '{}'", path.display())]
    ReceiptDecode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to resolve brewrs stages directory")]
    StagesResolve {
        #[source]
        source: io::Error,
    },
    #[error("failed to create cask removal quarantine")]
    RemovalQuarantine {
        #[source]
        source: io::Error,
    },
    #[error("failed to create cask upgrade quarantine")]
    UpgradeQuarantine {
        #[source]
        source: io::Error,
    },
    #[error("failed to run /usr/bin/ditto into custom --appdir")]
    DittoSpawn {
        #[source]
        source: io::Error,
    },
    #[error("/usr/bin/ditto failed copying app bundle to --appdir ({status})")]
    DittoFailed { status: ExitStatus },
    #[error("failed to install app bundle '{}' into '{}'", from.display(), to.display())]
    InstallApp {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to move '{}' to temporary removal path '{}'", from.display(), to.display())]
    MoveToSibling {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to quarantine '{}' during cask removal", path.display())]
    Quarantine {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("internal error: applied cask record missing after push")]
    AppliedRecordMissing,

    // ---- receipts: invalid or corrupt ----
    #[error("refusing '{}': cask receipt must be a regular file", path.display())]
    ReceiptNotRegular { path: PathBuf },
    #[error("refusing '{}': cask receipt is too large", path.display())]
    ReceiptTooLarge { path: PathBuf },
    #[error("cask receipt '{}' names '{actual}', not '{expected}'", path.display())]
    ReceiptNameMismatch {
        path: PathBuf,
        actual: String,
        expected: String,
    },
    #[error(transparent)]
    InvalidIdentifier(#[from] IdentifierError),
    #[error("invalid version '{version}' in cask receipt '{token}'")]
    InvalidVersion { version: String, token: String },
    #[error("invalid sha256 in cask receipt '{token}'")]
    InvalidSha256 { token: String },
    #[error("invalid app record in cask receipt '{token}'")]
    InvalidAppRecord { token: String },
    #[error("invalid wrapper sha256 in cask receipt '{token}'")]
    InvalidWrapperSha256 { token: String },
    #[error("invalid path '{path}' in cask receipt '{token}'")]
    InvalidPath { path: String, token: String },
    #[error("unsafe path '{path}' in cask receipt '{token}'")]
    UnsafePath { path: String, token: String },
    #[error("invalid app path '{}' in receipt", path.display())]
    InvalidAppPath { path: PathBuf },
    #[error("refusing to remove cask '{token} {version}': receipt '{}' could not be read", path.display())]
    ReceiptUnreadable {
        token: String,
        version: String,
        path: PathBuf,
        #[source]
        source: Box<CaskStoreError>,
    },
    #[error("refusing to remove cask '{token} {version}': receipt changed on disk")]
    ReceiptChanged { token: String, version: String },

    // ---- unsafe destinations and paths ----
    #[error("refusing unsafe cask destination '{dest}'")]
    UnsafeDestination { dest: String },
    #[error("refusing cask destination '{dest}': conflicts with existing path '{}'", path.display())]
    DestinationConflict { dest: String, path: PathBuf },
    #[error("empty prefix relative destination")]
    EmptyDestination,
    #[error("refusing non-relative destination '{dest}'")]
    NonRelativeDestination { dest: String },
    #[error("refusing unsafe destination '{dest}' in receipt")]
    UnsafeReceiptDestination { dest: String },
    #[error("missing directory '{}' for '{dest}'", path.display())]
    MissingParentDirectory {
        path: PathBuf,
        dest: String,
        #[source]
        source: io::Error,
    },
    #[error("refusing '{}': parent directory is not a real directory", path.display())]
    ParentNotRealDirectory { path: PathBuf },
    #[error("app destination has no parent directory")]
    AppDestinationNoParent,
    #[error("app bundle path has no parent directory")]
    AppBundleNoParent,
    #[error("refusing cask stage '{}' outside brewrs stages", stage.display())]
    StageOutside { stage: PathBuf },
    #[error("refusing to overwrite existing path '{}'", path.display())]
    PathExists { path: PathBuf },
    #[error("unexpected existing Caskroom directory '{}'", path.display())]
    UnexpectedCaskroomDirectory { path: PathBuf },
    #[error("staged app bundle missing from cask receipt")]
    StagedAppNotInReceipt,

    // ---- --appdir ----
    #[error("refusing --appdir '{}': use an absolute directory path", path.display())]
    AppdirNotAbsolute { path: PathBuf },
    #[error("refusing --appdir '{}': directory does not exist or cannot be inspected", path.display())]
    AppdirInaccessible {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("refusing --appdir '{}': must be a real directory, not a symlink", path.display())]
    AppdirNotRealDirectory { path: PathBuf },
    #[error("appdir '{}' is no longer a real directory", path.display())]
    AppdirChanged { path: PathBuf },

    // ---- file verification ----
    #[error("missing file '{}'", path.display())]
    MissingFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("'{}' is not a regular file", path.display())]
    NotRegularFile { path: PathBuf },
    #[error("SHA-256 mismatch at '{}': expected {expected}, got {actual}", path.display())]
    Sha256Mismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    // ---- conflicts and duplicates ----
    #[error(
        "cask '{token}' conflicts with '{conflict}'; remove '{conflict}' before installing '{token}'"
    )]
    CaskConflict { token: String, conflict: String },
    #[error(
        "cask '{token}' conflicts with formula '{conflict}'; remove '{conflict}' before installing '{token}'"
    )]
    FormulaConflict { token: String, conflict: String },
    #[error("cask '{token}' appears more than once in one commit")]
    DuplicateInCommit { token: String },
    #[error("cask '{token}' appears more than once in one upgrade")]
    DuplicateInUpgrade { token: String },
    #[error("cask '{token}' appears more than once in removal list")]
    DuplicateInRemoval { token: String },
    #[error("duplicate .app destination '{}' in one commit", path.display())]
    DuplicateAppDestination { path: PathBuf },
    #[error("duplicate cask destination '{dest}' in one commit")]
    DuplicateDestination { dest: String },
    #[error("cask '{token}' is already installed")]
    AlreadyInstalled { token: String },
    #[error("refusing cask upgrade from '{from}' to mismatched token '{to}'")]
    UpgradeTokenMismatch { from: String, to: String },
    #[error(
        "refusing to upgrade cask '{token}' in place at identical version '{version}' and sha256"
    )]
    UpgradeIdentical { token: String, version: String },

    // ---- removal preflight and changes after preflight ----
    #[error("refusing to remove cask '{token}': owned symlink '{}' is missing", path.display())]
    OwnedSymlinkMissing {
        token: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("refusing to remove cask '{token}': '{}' is no longer a symlink", path.display())]
    NoLongerSymlink { token: String, path: PathBuf },
    #[error(
        "refusing to remove cask '{token}': symlink '{}' points to '{}' instead of '{expected}'",
        path.display(),
        actual.display()
    )]
    SymlinkRetargeted {
        token: String,
        path: PathBuf,
        actual: PathBuf,
        expected: String,
    },
    #[error("refusing to remove cask '{token}': command_wrapper '{}' was modified or removed", path.display())]
    WrapperModified {
        token: String,
        path: PathBuf,
        #[source]
        source: Box<CaskStoreError>,
    },
    #[error("refusing to remove cask '{token}': app bundle '{}' is missing", path.display())]
    AppBundleMissing {
        token: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("refusing to remove cask '{token}': '{}' is not a real .app directory", path.display())]
    NotAppDirectory { token: String, path: PathBuf },
    #[error("refusing to remove cask '{token}': Caskroom version '{}' is missing", path.display())]
    CaskroomVersionMissing {
        token: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("refusing to remove cask '{token}': '{}' is not a real directory", path.display())]
    NotRealDirectory { token: String, path: PathBuf },
    #[error("cask symlink '{}' changed before removal", path.display())]
    SymlinkChanged { path: PathBuf },
    #[error("cask directory '{}' is no longer a real directory", path.display())]
    DirectoryChanged { path: PathBuf },

    // ---- commit, upgrade and rollback failures ----
    #[error("cask rollback was incomplete: {}", issues.join("; "))]
    CommitRollbackIncomplete {
        issues: Vec<String>,
        #[source]
        source: Box<CaskStoreError>,
    },
    #[error("cask removal rollback was incomplete: {}", issues.join("; "))]
    RemovalRollbackIncomplete {
        issues: Vec<String>,
        #[source]
        source: Box<CaskStoreError>,
    },
    #[error(
        "upgrade preflight rollback failed: {}; quarantine kept at '{}'",
        issues.join("; "),
        kept.display()
    )]
    UpgradePreflightRollbackFailed {
        issues: Vec<String>,
        kept: PathBuf,
        #[source]
        source: Box<CaskStoreError>,
    },
    #[error(
        "cask upgrade rollback was incomplete: {}; quarantine kept at '{}'",
        issues.join("; "),
        kept.display()
    )]
    UpgradeRollbackIncomplete {
        issues: Vec<String>,
        kept: PathBuf,
        #[source]
        source: Box<CaskStoreError>,
    },
}

impl CaskStoreError {
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
