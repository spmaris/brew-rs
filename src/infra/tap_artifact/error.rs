//! Typed failures of tap artifact staging and verification.

use std::io;
use std::path::PathBuf;

/// Why a tap artifact was rejected or could not be staged or verified.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TapArtifactError {
    // --- I/O ---
    #[error("failed to {action} {what} '{}'", .path.display())]
    Io {
        action: &'static str,
        what: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to {action} '{}'", .path.display())]
    IoPath {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to make '{}' executable", .path.display())]
    MakeExecutable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("tap artifact size overflowed")]
    SizeOverflow,

    // --- staging ---
    #[error("refusing to stage a tap artifact without a lowercase SHA-256 digest")]
    StageWithoutDigest,
    #[error("refusing to stage a tap artifact for unsupported architecture '{arch}'")]
    StageUnsupportedArch { arch: String },
    #[error("refusing to stage '{}': the cached artifact is a symlink", .path.display())]
    StageSymlink { path: PathBuf },
    #[error("refusing to stage '{}': the cached artifact is not a regular file", .path.display())]
    StageNotRegular { path: PathBuf },
    #[error("refusing to stage '{}': size {size} is outside the supported range", .path.display())]
    StageSizeOutOfRange { path: PathBuf, size: u64 },
    #[error("refusing to stage '{}': the cached artifact changed while it was opened", .path.display())]
    StageChangedWhileOpened { path: PathBuf },
    #[error("refusing to stage '{}': artifact exceeds the {limit} byte limit", .path.display())]
    StageTooLarge { path: PathBuf, limit: u64 },
    #[error("SHA-256 mismatch for '{}': expected {expected}, got {actual}", .path.display())]
    StageDigestMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    // --- verification ---
    #[error("refusing to verify {what} without a lowercase SHA-256 digest")]
    VerifyWithoutDigest { what: &'static str },
    #[error("refusing to verify {what} '{}': it is a symlink", .path.display())]
    VerifySymlink { what: &'static str, path: PathBuf },
    #[error("refusing to verify {what} '{}': it is not a regular file", .path.display())]
    VerifyNotRegular { what: &'static str, path: PathBuf },
    #[error(
        "refusing to verify {what} '{}': {size} bytes exceeds the {limit} byte limit",
        .path.display()
    )]
    VerifySizeExceeded {
        what: &'static str,
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    #[error("refusing to verify {what} '{}': not a regular file", .path.display())]
    VerifyOpenedNotRegular { what: &'static str, path: PathBuf },
    #[error("refusing to verify {what} '{}': it exceeds the {limit} byte limit", .path.display())]
    VerifyStreamExceeded {
        what: &'static str,
        path: PathBuf,
        limit: u64,
    },
    #[error("SHA-256 mismatch for {what} '{}': expected {expected}, got {actual}", .path.display())]
    VerifyDigestMismatch {
        what: &'static str,
        path: PathBuf,
        expected: String,
        actual: String,
    },

    // --- Mach-O header ---
    #[error("Mach-O header is too short to read offset {offset}")]
    HeaderTooShort { offset: usize },
    #[error("unsupported tap artifact architecture '{arch}'")]
    UnsupportedArch { arch: String },
    #[error("tap artifact '{}' is too short to be a Mach-O executable", .path.display())]
    TooShort {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "refusing '{}': universal (fat) Mach-O artifacts are unsupported; provide one naked executable per architecture",
        .path.display()
    )]
    FatMacho { path: PathBuf },
    #[error(
        "refusing '{}': expected a thin 64-bit little-endian Mach-O executable",
        .path.display()
    )]
    NotThin64 { path: PathBuf },
    #[error(
        "refusing '{}': Mach-O CPU type 0x{cpu_type:08x} does not match the selected {arch} artifact",
        .path.display()
    )]
    CpuMismatch {
        path: PathBuf,
        cpu_type: u32,
        arch: String,
    },
    #[error(
        "refusing '{}': Mach-O file type 0x{file_type:08x} is not a standalone executable",
        .path.display()
    )]
    NotExecutableType { path: PathBuf, file_type: u32 },
}
