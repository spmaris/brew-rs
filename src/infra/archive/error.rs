//! Typed failures of bounded bottle extraction.

use std::io;
use std::path::PathBuf;

use tar::EntryType;

/// Why a bottle archive was rejected or could not be extracted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ArchiveError {
    // --- I/O ---
    #[error("failed to {action} {}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to {action}")]
    Stream {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to read the mode of '{}'", .path.display())]
    ModeRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    // --- arguments ---
    #[error("bottle sha256 must be exactly {len} lowercase hexadecimal characters")]
    InvalidDigest { len: usize },
    #[error("bottle name must be between 1 and {max} bytes")]
    InvalidNameLength { max: usize },
    #[error("bottle name '{name}' must start with a lowercase letter or digit")]
    InvalidNameStart { name: String },
    #[error("bottle name '{name}' contains unsupported characters")]
    InvalidNameChars { name: String },
    #[error("bottle version must be between 1 and {max} bytes")]
    InvalidVersionLength { max: usize },
    #[error("bottle version '{version}' contains unsupported characters")]
    InvalidVersionChars { version: String },

    // --- staging directory and snapshot ---
    #[error("staging path {} is not a regular directory", .path.display())]
    StagingNotDirectory { path: PathBuf },
    #[error("staging directory {} is not empty; refusing to extract", .path.display())]
    StagingNotEmpty { path: PathBuf },
    #[error("bottle archive {} is not a regular file", .path.display())]
    NotRegularFile { path: PathBuf },
    #[error("bottle archive {} is empty or exceeds the {limit} byte limit", .path.display())]
    ArchiveSizeOutOfRange { path: PathBuf, limit: u64 },
    #[error("bottle archive {} changed while it was being opened", .path.display())]
    ChangedWhileOpening { path: PathBuf },
    #[error("bottle archive {} was replaced while it was being opened", .path.display())]
    ReplacedWhileOpening { path: PathBuf },
    #[error("bottle archive size overflowed")]
    ArchiveSizeOverflow,
    #[error("bottle archive {} exceeds the {limit} byte limit", .path.display())]
    ArchiveTooLarge { path: PathBuf, limit: u64 },
    #[error("bottle archive {} changed while it was being read", .path.display())]
    ChangedWhileReading { path: PathBuf },
    #[error("SHA-256 mismatch for the bottle archive: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },

    // --- stream framing ---
    #[error("unexpected data after the tar end marker")]
    DataAfterTarEnd,
    #[error("unexpected bytes after the gzip stream")]
    BytesAfterGzip,

    // --- entry paths ---
    #[error("bottle archive contains an empty path")]
    EmptyPath,
    #[error("bottle archive path '{path}' contains a backslash, NUL, or control byte")]
    PathControlBytes { path: String },
    #[error("bottle archive path is not valid UTF-8")]
    PathNotUtf8,
    #[error("bottle archive path '{path}' is not a normalized relative path")]
    PathNotNormalized { path: String },
    #[error("bottle archive path '{path}' contains an overlong component")]
    PathOverlongComponent { path: String },
    #[cfg(windows)]
    #[error("bottle archive path '{path}' contains a Windows drive prefix")]
    PathDrivePrefix { path: String },
    #[error("bottle archive path '{path}' is outside '{expected}'")]
    UnexpectedRoot { path: String, expected: String },
    #[error("bottle archive path '{path}' is not a directory")]
    PathNotDirectory { path: String },
    #[error("bottle archive file path '{path}' ends with a slash")]
    FilePathTrailingSlash { path: String },
    #[error("bottle archive path '{}' is nested under an extracted file", .path.display())]
    NestedUnderFile { path: PathBuf },
    #[error("bottle archive path '{}' is nested under an extracted symlink", .path.display())]
    NestedUnderSymlink { path: PathBuf },
    #[error("extraction path {} is not a regular directory", .path.display())]
    ExtractionPathNotDirectory { path: PathBuf },
    #[error("bottle archive entry '{}' exceeds the depth limit of {limit}", .path.display())]
    EntryTooDeep { path: PathBuf, limit: usize },
    #[error("bottle archive contains duplicate path '{}'", .path.display())]
    DuplicatePath { path: PathBuf },

    // --- entry kinds and limits ---
    #[error("unsupported bottle archive entry type {entry_type:?}")]
    UnsupportedEntryType { entry_type: EntryType },
    #[error("bottle archive entry count overflowed")]
    EntryCountOverflow,
    #[error("bottle archive has more than {limit} entries")]
    TooManyEntries { limit: usize },
    #[error("bottle archive entry '{}' exceeds the {limit} byte limit", .path.display())]
    EntryTooLarge { path: PathBuf, limit: u64 },
    #[error("bottle archive expanded size overflowed")]
    ExpandedSizeOverflow,
    #[error("bottle archive expands beyond the {limit} byte limit")]
    ExpandedTooLarge { limit: u64 },
    #[error(
        "bottle archive entry '{}' was truncated: expected {expected} bytes, got {actual}",
        .path.display()
    )]
    EntryTruncated {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },

    // --- symlinks ---
    #[error("bottle archive symlink '{}' is used as an ancestor directory", .path.display())]
    SymlinkAncestor { path: PathBuf },
    #[error("bottle archive symlink '{}' declares a nonzero size", .path.display())]
    SymlinkNonzeroSize { path: PathBuf },
    #[error("bottle archive symlink '{}' has no target", .path.display())]
    SymlinkNoTarget { path: PathBuf },
    #[error("bottle archive symlink '{}' has a target longer than {limit} bytes", .path.display())]
    SymlinkTargetTooLong { path: PathBuf, limit: usize },
    #[error("bottle archive has more than {limit} symlinks")]
    TooManySymlinks { limit: usize },
    #[error("bottle archive symlink '{}' has an empty target", .link.display())]
    SymlinkEmptyTarget { link: PathBuf },
    #[error(
        "bottle archive symlink '{}' has a target containing a backslash, NUL, or control byte",
        .link.display()
    )]
    SymlinkTargetControlBytes { link: PathBuf },
    #[error("bottle archive symlink '{}' has a target that is not valid UTF-8", .link.display())]
    SymlinkTargetNotUtf8 { link: PathBuf },
    #[error("bottle archive symlink '{}' has an absolute target", .link.display())]
    SymlinkAbsoluteTarget { link: PathBuf },
    #[cfg(windows)]
    #[error("bottle archive symlink '{}' has a Windows absolute or drive target", .link.display())]
    SymlinkWindowsTarget { link: PathBuf },
    #[error("bottle archive symlink '{}' has a target with too many components", .link.display())]
    SymlinkTooManyComponents { link: PathBuf },
    #[error("bottle archive symlink '{}' has a target with an empty component", .link.display())]
    SymlinkEmptyComponent { link: PathBuf },
    #[error("bottle archive symlink '{}' has a target with an overlong component", .link.display())]
    SymlinkOverlongComponent { link: PathBuf },
    #[error("bottle archive symlink path is not valid UTF-8")]
    SymlinkPathNotUtf8,
    #[error(
        "bottle archive symlink '{}' has a target that uses '..' after a normal component",
        .link.display()
    )]
    SymlinkParentAfterNormal { link: PathBuf },
    #[error("bottle archive symlink '{}' escapes the keg root", .link.display())]
    SymlinkEscapesRoot { link: PathBuf },
    #[error(
        "bottle archive symlink '{}' has a target that exceeds the depth limit of {limit}",
        .link.display()
    )]
    SymlinkTargetTooDeep { link: PathBuf, limit: usize },
    #[error("bottle archive symlink '{}' resolves to the keg root", .link.display())]
    SymlinkResolvesToRoot { link: PathBuf },
    #[error("bottle archive symlink '{}' targets a path through a regular file", .link.display())]
    SymlinkThroughFile { link: PathBuf },
    #[error(
        "bottle archive symlink '{}' targets an ancestor directory, which would form a cycle",
        .link.display()
    )]
    SymlinkAncestorCycle { link: PathBuf },
    #[error("bottle archive symlink '{}' forms a symlink cycle", .link.display())]
    SymlinkCycle { link: PathBuf },
    #[error(
        "bottle archive symlink '{}' exceeds the symlink resolution depth limit of {limit}",
        .link.display()
    )]
    SymlinkHopLimit { link: PathBuf, limit: usize },
    #[error(
        "bottle archive symlink '{}' references unresolved symlink '{}'",
        .link.display(),
        .target.display()
    )]
    SymlinkUnresolved { link: PathBuf, target: PathBuf },
    #[error(
        "bottle archive symlink '{}' targets '{}', which is not staged in the archive",
        .link.display(),
        .target.display()
    )]
    SymlinkNotStaged { link: PathBuf, target: PathBuf },
    #[error("bottle archive symlink '{}' resolves to an empty path", .link.display())]
    SymlinkEmptyPath { link: PathBuf },
    #[error(
        "extraction path {} already exists; refusing to replace it with a symlink",
        .path.display()
    )]
    SymlinkDestinationExists { path: PathBuf },
    #[cfg(not(unix))]
    #[error("bottle archive symlink '{}' cannot be created on this platform", .link.display())]
    SymlinkUnsupportedPlatform { link: PathBuf },
}
