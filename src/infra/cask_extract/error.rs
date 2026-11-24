//! Typed failures of cask payload detection, extraction and staging.

use std::io;
use std::num::{ParseIntError, TryFromIntError};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::str::Utf8Error;

use tar::EntryType;

use crate::infra::download::DownloadError;

/// Why a cask payload was rejected or could not be staged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CaskExtractError {
    // --- I/O ---
    #[error("failed to {action} '{}'", .path.display())]
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
    /// A filesystem error that carried no extra context before the typed
    /// conversion and keeps its original text.
    #[error("failed to create staged symlink '{}' -> '{target}'", .path.display())]
    SymlinkCreate {
        path: PathBuf,
        target: String,
        #[source]
        source: io::Error,
    },

    // --- digest and stage ---
    #[error("SHA-256 verification failed before staging cask {token} {version}")]
    DigestVerification {
        token: String,
        version: String,
        #[source]
        source: DownloadError,
    },
    #[error("cask stage '{}' is not a real directory", .path.display())]
    StageNotDirectory { path: PathBuf },

    // --- format detection and naked payloads ---
    #[error("downloaded cask '{}' is empty or not a regular file", .path.display())]
    EmptyOrNotRegular { path: PathBuf },
    #[error("unrecognized cask payload format for '{url_path}'")]
    UnsupportedFormat { url_path: String },
    #[error("cask declares app bundle '{bundle_name}' but downloaded payload is a naked file")]
    NakedAppBundle { bundle_name: String },
    #[error("cask references $APPDIR bundle in a naked file payload")]
    NakedAppdirReference,
    #[error("naked cask payload must map to exactly one payload file (got {count})")]
    NakedPayloadCount { count: usize },

    // --- tar ---
    #[error("cask tarball exceeds {limit} entries")]
    TarballTooManyEntries { limit: usize },
    #[error("cask tarball entry has a non-UTF-8 or invalid path")]
    TarballEntryPath {
        #[source]
        source: io::Error,
    },
    #[error("cask tarball entry '{}' exceeds size limit", .path.display())]
    TarballEntryTooLarge { path: PathBuf },
    #[error("tar symlink missing link target")]
    TarSymlinkNoTarget,
    #[error("tar symlink target is not UTF-8")]
    TarSymlinkTargetNotUtf8,
    #[error(
        "cask tarball entry '{}' uses unsupported entry type {entry_type:?}",
        .path.display()
    )]
    TarballEntryType {
        path: PathBuf,
        entry_type: EntryType,
    },
    #[error("/usr/bin/tar -tf failed ({status})")]
    TarListFailed { status: ExitStatus },
    #[error("tarball contains non-UTF-8 entry paths")]
    TarListingNotUtf8 {
        #[source]
        source: Utf8Error,
    },
    #[error("tarball exceeds {limit} entries")]
    TarTooManyEntries { limit: usize },
    #[error("/usr/bin/tar extraction failed ({status})")]
    TarExtractFailed { status: ExitStatus },

    // --- extracted tree and entry paths ---
    #[error("extracted cask exceeds maximum directory depth ({limit})")]
    TreeTooDeep { limit: usize },
    #[error("extracted cask exceeds {limit} entries")]
    TreeTooManyEntries { limit: usize },
    #[error("non-UTF-8 symlink target at '{}'", .path.display())]
    TreeSymlinkTargetNotUtf8 { path: PathBuf },
    #[error("extracted file '{}' exceeds size limit", .path.display())]
    ExtractedFileTooLarge { path: PathBuf },
    #[error("extracted cask byte size overflowed")]
    ExtractedSizeOverflow,
    #[error("extracted cask exceeds {limit} bytes")]
    ExtractedTooLarge { limit: u64 },
    #[error("refusing extracted special file at '{}'", .path.display())]
    SpecialFile { path: PathBuf },
    #[error("invalid symlink target at '{}'", .link.display())]
    InvalidSymlinkTarget { link: PathBuf },
    #[error("refusing absolute symlink '{}' -> '{target}'", .link.display())]
    AbsoluteSymlink { link: PathBuf, target: String },
    #[error("refusing escaping symlink '{}' -> '{target}'", .link.display())]
    EscapingSymlink { link: PathBuf, target: String },
    #[error("refusing non-relative symlink '{}' -> '{target}'", .link.display())]
    NonRelativeSymlink { link: PathBuf, target: String },
    #[error("archive contains empty entry path")]
    EmptyEntryPath,
    #[error("archive contains unsafe entry path '{path}'")]
    UnsafeEntryPath { path: String },
    #[error("archive contains Windows drive path '{path}'")]
    WindowsDrivePath { path: String },
    #[error("archive contains disallowed path component in '{path}'")]
    DisallowedPathComponent { path: String },
    #[error("archive entry '{path}' exceeds maximum depth {limit}")]
    EntryTooDeep { path: String, limit: usize },
    #[error("archive entry path contains non-UTF-8 characters")]
    EntryPathNotUtf8,
    #[error("archive contains disallowed path '{path}'")]
    DisallowedPath { path: String },

    // --- zip ---
    #[error("ZIP archive '{}' has invalid size {size}", .path.display())]
    ZipInvalidSize { path: PathBuf, size: u64 },
    #[error("ZIP archive is missing a valid EOCD record")]
    ZipMissingEocd,
    #[error("multi-disk ZIP archives are not supported")]
    ZipMultiDisk,
    #[error("ZIP archive contains {count} entries, above the {limit} limit")]
    ZipTooManyEntries { count: u64, limit: usize },
    #[error("ZIP central directory size overflow")]
    ZipDirectoryOverflow,
    #[error("ZIP central directory extends beyond archive bounds")]
    ZipDirectoryOutOfBounds,
    #[error("ZIP central directory too large")]
    ZipDirectoryTooLarge {
        #[source]
        source: TryFromIntError,
    },
    #[error("truncated ZIP central directory entry")]
    ZipTruncatedEntry,
    #[error("invalid ZIP central directory entry signature")]
    ZipInvalidEntrySignature,
    #[error("truncated ZIP entry metadata")]
    ZipTruncatedMetadata,
    #[error("ZIP archive contains a non-UTF-8 entry name")]
    ZipNameNotUtf8 {
        #[source]
        source: Utf8Error,
    },
    #[error("ZIP64 entry is missing uncompressed size in extra field")]
    ZipMissingZip64Size,
    #[error("ZIP entry '{name}' exceeds the {limit} byte limit")]
    ZipEntryTooLarge { name: String, limit: u64 },
    #[error("ZIP total uncompressed size overflowed")]
    ZipSizeOverflow,
    #[error("ZIP archive uncompressed size exceeds {limit} bytes")]
    ZipTooLarge { limit: u64 },
    #[error("invalid ZIP64 EOCD locator offset")]
    Zip64InvalidLocatorOffset,
    #[error("missing ZIP64 EOCD locator")]
    Zip64MissingLocator,
    #[error("ZIP64 EOCD offset is out of bounds")]
    Zip64OffsetOutOfBounds,
    #[error("invalid ZIP64 EOCD signature")]
    Zip64InvalidSignature,
    #[error("unexpected EOF reading u{bits}")]
    ZipUnexpectedEof { bits: u8 },
    #[error("/usr/bin/ditto failed to extract '{}' ({status})", .path.display())]
    DittoExtractFailed { path: PathBuf, status: ExitStatus },

    // --- dmg ---
    #[error("cask '{token}' has no extractable DMG source artifacts")]
    NoDmgSources { token: String },
    #[error("DMG artifact '{artifact}' is missing at '{}'", .path.display())]
    DmgArtifactMissing {
        artifact: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "refusing DMG artifact '{artifact}': path component '{}' is a symlink",
        .component.display()
    )]
    DmgArtifactSymlink {
        artifact: String,
        component: PathBuf,
    },
    #[error("/usr/bin/ditto failed to copy DMG artifact '{artifact}' ({status})")]
    DittoCopyFailed {
        artifact: String,
        status: ExitStatus,
    },
    #[error(
        "/usr/bin/hdiutil attach failed for '{}' ({status}): {stderr}",
        .path.display()
    )]
    HdiutilAttachFailed {
        path: PathBuf,
        status: ExitStatus,
        stderr: String,
    },
    #[error("hdiutil attach -plist returned non-UTF-8 output")]
    HdiutilOutputNotUtf8 {
        #[source]
        source: Utf8Error,
    },
    #[error("refusing DMG mount point '{}' outside '{}'", .mount_point.display(), .parent.display())]
    MountOutsideParent {
        mount_point: PathBuf,
        parent: PathBuf,
    },
    #[error("failed to detach DMG at '{}'", .mount_point.display())]
    DetachFailed { mount_point: PathBuf },
    #[error("invalid mount-point path in hdiutil plist: '{value}'")]
    InvalidMountPoint { value: String },
    #[error("hdiutil attach output did not include a <key>mount-point</key>")]
    MissingMountPoint,
    #[error("unterminated XML entity in hdiutil plist")]
    UnterminatedXmlEntity,
    #[error("invalid hex XML entity in hdiutil plist")]
    InvalidHexEntity {
        #[source]
        source: ParseIntError,
    },
    #[error("invalid decimal XML entity in hdiutil plist")]
    InvalidDecimalEntity {
        #[source]
        source: ParseIntError,
    },
    #[error("invalid Unicode scalar in XML entity")]
    InvalidUnicodeScalar,
    #[error("unsupported XML entity '&{entity};' in hdiutil plist")]
    UnsupportedXmlEntity { entity: String },

    // --- assembly ---
    #[error("cask '{token}' declared app '{app}' which was not found in the archive")]
    AppNotFound {
        token: String,
        app: String,
        #[source]
        source: io::Error,
    },
    #[error("cask '{token}' app '{app}' is not a regular .app directory")]
    AppNotBundle { token: String, app: String },
    #[error("cask '{token}' app '{app}' is missing a Contents directory")]
    AppMissingContents { token: String, app: String },
    #[error("failed to stage app bundle '{bundle_name}' into '{}'", .dest.display())]
    StageApp {
        bundle_name: String,
        dest: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("referenced app bundle '{bundle_name}' was not staged")]
    BundleNotStaged { bundle_name: String },
    #[error("missing declared payload artifact '{rel}'")]
    PayloadMissing {
        rel: String,
        #[source]
        source: io::Error,
    },
    #[error("declared payload artifact '{rel}' is not a regular file")]
    PayloadNotRegular { rel: String },
    #[error("extracted artifact '{rel}' not found in archive")]
    ArtifactNotFound {
        rel: String,
        #[source]
        source: io::Error,
    },
    #[error("refusing symlink along declared artifact path '{rel}'")]
    ArtifactSymlink { rel: String },
    #[error("path '{inner_path}' escapes bundle '{}'", .bundle.display())]
    BundleEscape { inner_path: String, bundle: PathBuf },
    #[error("missing file '{inner_path}' inside bundle '{}'", .bundle.display())]
    BundleFileMissing {
        inner_path: String,
        bundle: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("symlink loop resolving '{inner_path}' in bundle")]
    BundleSymlinkLoop { inner_path: String },
    #[error("non-UTF-8 symlink inside .app bundle")]
    BundleSymlinkNotUtf8,
    #[error("non-directory component along '{inner_path}' in '{}'", .bundle.display())]
    BundleNonDirectory { inner_path: String, bundle: PathBuf },
    #[error("missing target '{inner_path}' in bundle '{}'", .bundle.display())]
    BundleTargetMissing {
        inner_path: String,
        bundle: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("target '{inner_path}' inside '{}' is not a regular file", .bundle.display())]
    BundleTargetNotRegular { inner_path: String, bundle: PathBuf },
}

impl CaskExtractError {
    /// A `map_err` adapter that names the action and path of a failed I/O call.
    pub(super) fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}
