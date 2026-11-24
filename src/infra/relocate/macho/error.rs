//! Typed failures of Mach-O parsing, planning and rewriting.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::str::Utf8Error;

use super::super::error::RelocateError;

/// Why a Mach-O image could not be parsed or relocated.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MachoError {
    // --- header and framing ---
    #[error("File is too small to be a Mach-O image")]
    TooSmall,
    #[error("FAT Mach-O images are unsupported; only thin 64-bit images can be relocated")]
    Fat,
    #[error("Big-endian 64-bit Mach-O images are unsupported")]
    BigEndian,
    #[error("32-bit Mach-O images are unsupported")]
    Bit32,
    #[error("File is not a little-endian 64-bit Mach-O image")]
    NotMacho,
    #[error("Truncated 64-bit Mach-O header")]
    TruncatedHeader,
    #[error("Unsupported Mach-O CPU type {cputype:#x}; expected arm64 or x86_64")]
    UnsupportedCpu { cputype: u32 },
    #[error("Unsupported Mach-O load-command count {count}")]
    BadCommandCount { count: u32 },
    #[error("Mach-O load-command table size overflows")]
    TableSizeOverflow,
    #[error("Mach-O load-command table ({bytes} bytes) extends past the end of the file")]
    TablePastEof { bytes: usize },
    #[error("Mach-O load-command table is too small for {count} commands")]
    TableTooSmall { count: usize },
    #[error("Mach-O load command {index} offset overflows")]
    CommandOffsetOverflow { index: usize },
    #[error("Truncated Mach-O load command {index}")]
    TruncatedCommand { index: usize },
    #[error(
        "Mach-O load command {index} has invalid size {size}; 64-bit commands are 8-byte aligned"
    )]
    InvalidCommandSize { index: usize, size: usize },
    #[error("Mach-O load command {index} size overflows")]
    CommandSizeOverflow { index: usize },
    #[error("Mach-O load command {index} extends past the load-command table")]
    CommandPastTable { index: usize },
    #[error("Mach-O image contains more than one LC_CODE_SIGNATURE command")]
    MultipleSignatures,
    #[error("LC_CODE_SIGNATURE has unexpected size {size}")]
    SignatureSize { size: usize },
    #[error("LC_CODE_SIGNATURE data range overflows")]
    SignatureRangeOverflow,
    #[error("LC_CODE_SIGNATURE data lies outside the image")]
    SignatureOutside,
    #[error("Mach-O load-command sizes sum to {actual} bytes, but the header declares {declared}")]
    CommandSizeSum { actual: usize, declared: usize },
    #[error("Mach-O field offset overflows")]
    FieldOffsetOverflow,
    #[error("Truncated Mach-O field at offset {offset}")]
    TruncatedField { offset: usize },

    // --- command paths ---
    #[error("{label} command is too small ({size} bytes)")]
    CommandTooSmall { label: &'static str, size: usize },
    #[error("{label} has invalid path offset {offset} in a {size}-byte command")]
    InvalidPathOffset {
        label: &'static str,
        offset: usize,
        size: usize,
    },
    #[error("{label} has non-zero bytes between its struct and path")]
    NonZeroBeforePath { label: &'static str },
    #[error("{label} path is not NUL-terminated")]
    PathNotTerminated { label: &'static str },
    #[error("{label} has an empty path")]
    EmptyPath { label: &'static str },
    #[error("{label} path is followed by non-zero padding")]
    NonZeroPadding { label: &'static str },
    #[error("{label} path is {len} bytes, over the {max}-byte limit")]
    PathTooLong {
        label: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{label} path is not valid UTF-8")]
    PathNotUtf8 {
        label: &'static str,
        #[source]
        source: Utf8Error,
    },
    #[error("{label} path is empty")]
    PathIsEmpty { label: &'static str },
    #[error("{label} path {name:?} starts with '-' and cannot be passed to install_name_tool")]
    NameLeadingDash { label: &'static str, name: String },
    #[error("{label} path {name:?} contains a control character")]
    NameControl { label: &'static str, name: String },
    #[error("{label} replacement path {name:?} still contains a Homebrew placeholder")]
    ReplacementMarker { label: &'static str, name: String },

    // --- command set ---
    #[error(
        "Unsupported Mach-O file type {filetype:#x}; only executables, dylibs, and bundles are supported"
    )]
    UnsupportedFileType { filetype: u32 },
    #[error("Load command {index} is missing its path")]
    CommandMissingPath { index: usize },
    #[error("Mach-O image has more than one LC_ID_DYLIB (commands {previous} and {index})")]
    MultipleIdDylib { previous: usize, index: usize },
    #[error("Mach-O executable unexpectedly declares LC_ID_DYLIB at command {index}")]
    ExecutableIdDylib { index: usize },
    #[error("Duplicate LC_RPATH path {name:?} at commands {previous} and {index}")]
    DuplicateRpath {
        name: String,
        previous: usize,
        index: usize,
    },
    #[error("Duplicate dependent dylib path {name:?} at commands {previous} and {index}")]
    DuplicateDylib {
        name: String,
        previous: usize,
        index: usize,
    },

    // --- plan ---
    #[error("Could not map {label} path {old:?}")]
    MapPath {
        label: &'static str,
        old: String,
        #[source]
        source: Box<RelocateError>,
    },
    #[error("Path mapper returned inconsistent results for {old:?}: {previous:?} and {mapped:?}")]
    InconsistentMapper {
        old: String,
        previous: Option<String>,
        mapped: Option<String>,
    },
    #[error(
        "{label} path {target:?} still contains a Homebrew placeholder; the mapper must resolve it"
    )]
    UnresolvedPlaceholder { label: &'static str, target: String },
    #[error("No relocation target planned for load command {index}")]
    NoTargetPlanned { index: usize },
    #[error("Relocation would make load commands {previous} and {index} share the path {target:?}")]
    SharedPath {
        previous: usize,
        index: usize,
        target: String,
    },
    #[error(
        "Raw Homebrew placeholder at file offset {offset} is outside a supported Mach-O load command; refusing to guess at its meaning"
    )]
    PlaceholderOutsideCommand { offset: usize },
    #[error("Homebrew placeholder in load command {index} was not resolved ({target:?})")]
    PlaceholderNotResolved { index: usize, target: String },
    #[error("Rewritten image still contains a raw Homebrew placeholder at file offset {offset}")]
    PlaceholderRemains { offset: usize },

    // --- applying and verifying ---
    #[error("Could not run {program}")]
    ToolSpawn {
        program: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{context} ({status}): {detail}")]
    ToolFailed {
        context: &'static str,
        status: ExitStatus,
        detail: String,
    },
    #[error("Could not read {stage} image {}", .path.display())]
    ReadImage {
        stage: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("The image did not match the relocation plan after {stage}")]
    PlanMismatch {
        stage: &'static str,
        #[source]
        source: Box<MachoError>,
    },
    #[error("Rewritten file is not a supported thin 64-bit Mach-O image")]
    RewrittenUnsupported {
        #[source]
        source: Box<MachoError>,
    },
    #[error("Rewritten load-command set is not relocatable")]
    RewrittenNotRelocatable {
        #[source]
        source: Box<MachoError>,
    },
    #[error("Rewritten Mach-O header identity changed")]
    HeaderIdentityChanged,
    #[error("Load command {index} disappeared from the rewritten image")]
    CommandDisappeared { index: usize },
    #[error("Load command {index} changed type from {from:#x} to {to:#x}")]
    CommandTypeChanged { index: usize, from: u32, to: u32 },
    #[error(
        "Load command {index} expected path {expected:?}, but the rewritten image contains {actual:?}"
    )]
    PathMismatch {
        index: usize,
        expected: String,
        actual: String,
    },
    #[error("Load command {index} lost its path in the rewritten image")]
    PathLost { index: usize },
    #[error("Load command {index} unexpectedly gained the path {value:?} in the rewritten image")]
    PathGained { index: usize, value: String },
    #[error("Rewritten image gained an unexpected load command {command:#x}")]
    ExtraCommand { command: u32 },
    #[error("Could not inspect staged image {}", .path.display())]
    InspectImage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Staged image {} is not a regular file", .path.display())]
    ImageNotRegular { path: PathBuf },
    #[error("Could not re-read staged image {}", .path.display())]
    ReReadImage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Staged image {} changed after it was inspected; refusing to rewrite it", .path.display())]
    ImageChanged { path: PathBuf },
    #[error("Mach-O relocation requires macOS")]
    MacosRequired,
    #[error("macOS relocation failed; the original bytes of {} were restored", .path.display())]
    Restored {
        path: PathBuf,
        #[source]
        source: Box<MachoError>,
    },
    #[error(
        "macOS relocation failed and the original bytes of {} could not be restored: {restore_error}",
        .path.display()
    )]
    RestoreFailed {
        path: PathBuf,
        restore_error: io::Error,
        #[source]
        source: Box<MachoError>,
    },
}
