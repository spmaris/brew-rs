//! Typed refusals of tap names, manifests, registry records and receipts.
//!
//! Every `Display` text is the exact message the tap lane has always produced.

use std::str::Utf8Error;

use super::limits::{
    MAX_MANIFEST_BYTES, MAX_PACKAGES, MAX_SOURCE_PATH_BYTES, MAX_URL_BYTES, SCHEMA_VERSION,
};
use crate::domain::validate::UrlError;

/// Why a tap name, package token, executable name or version was refused.
/// `what` names the refused value (for example `package token`).
#[derive(Debug, thiserror::Error)]
pub(crate) enum NameError {
    #[error("tap target '{raw}' has an unsupported length")]
    TargetLength { raw: String },
    #[error("tap target '{raw}' is not fully qualified; use <owner>/<repo>/<token>")]
    NotQualified { raw: String },
    #[error("tap target '{raw}' has too many path components; use <owner>/<repo>/<token>")]
    TooManyComponents { raw: String },
    #[error("tap name '{raw}' has an unsupported length")]
    TapNameLength { raw: String },
    #[error("tap name '{raw}' must be <owner>/<repo>")]
    TapNameShape { raw: String },
    #[error("tap name '{raw}' must contain exactly one '/'")]
    TapNameSlash { raw: String },
    #[error("tap owner '{owner}' is reserved for official Homebrew taps")]
    ReservedOwner { owner: String },
    #[error("tap repository '{name}' is reserved")]
    ReservedRepository { name: String },
    #[error("{what} '{value}' is reserved")]
    Reserved { what: &'static str, value: String },
    #[error("{what} '{value}' must be between 1 and {max} characters")]
    Length {
        what: &'static str,
        value: String,
        max: usize,
    },
    #[error("{what} '{value}' must not contain '..'")]
    DotDot { what: &'static str, value: String },
    #[error("{what} '{value}' must not start or end with '.'")]
    EdgeDot { what: &'static str, value: String },
    #[error("{what} '{value}' must not end with '.'")]
    TrailingDot { what: &'static str, value: String },
    #[error("{what} '{value}' must start with a lowercase letter or digit")]
    StartLowerOrDigit { what: &'static str, value: String },
    #[error("{what} '{value}' must start with a lowercase letter, digit or '_'")]
    StartLowerDigitOrUnderscore { what: &'static str, value: String },
    #[error("{what} '{value}' must start with a letter or digit")]
    StartAlphanumeric { what: &'static str, value: String },
    #[error("{what} '{value}' may only contain lowercase letters, digits, '.', '_' and '-'")]
    ComponentCharacters { what: &'static str, value: String },
    #[error("{what} '{value}' may only contain [a-z0-9@+._-]")]
    IdentifierCharacters { what: &'static str, value: String },
    #[error("{what} '{value}' may only contain [A-Za-z0-9._+-]")]
    VersionCharacters { what: &'static str, value: String },
}

/// Why the host cannot select a tap artifact.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HostArchError {
    #[error("third-party tap artifacts are naked macOS executables; this host is {os}")]
    NotMacos { os: &'static str },
    #[error("unsupported host architecture '{arch}' for macOS tap artifacts")]
    UnsupportedArch { arch: &'static str },
}

/// Why a tap manifest was refused, or a package lookup in it failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManifestError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error("tap manifest exceeds the {MAX_MANIFEST_BYTES} byte limit")]
    TooLarge,
    #[error("tap manifest is not valid UTF-8")]
    NotUtf8(#[source] Utf8Error),
    #[error("failed to parse the tap manifest as TOML")]
    InvalidToml(#[source] toml::de::Error),
    #[error("unsupported tap manifest schema {found}; expected {SCHEMA_VERSION}")]
    UnsupportedSchema { found: u32 },
    #[error("manifest declares tap '{declared}', not '{expected}'")]
    TapMismatch { declared: String, expected: String },
    #[error("tap '{tap}' declares no packages")]
    NoPackages { tap: String },
    #[error("tap '{tap}' declares {count} packages, above the {MAX_PACKAGES} package limit")]
    TooManyPackages { tap: String, count: usize },
    #[error("tap '{tap}' declares package token '{token}' more than once")]
    DuplicateToken { tap: String, token: String },
    #[error("tap '{tap}' declares executable name '{bin}' more than once")]
    DuplicateBin { tap: String, bin: String },
    #[error("tap package '{token}' declares no architecture artifacts")]
    NoArtifacts { token: String },
    #[error(
        "tap package '{token}' declares {count} artifacts; only arm64 and x86_64 are supported"
    )]
    TooManyArtifacts { token: String, count: usize },
    #[error(
        "tap package '{token}' declares unsupported architecture '{architecture}'; expected arm64 or x86_64"
    )]
    UnsupportedArchitecture { token: String, architecture: String },
    #[error(
        "tap package '{token}' has an invalid sha256 for '{architecture}': expected 64 lowercase hexadecimal characters"
    )]
    InvalidSha256 { token: String, architecture: String },
    #[error("tap package '{token}' has a URL longer than {MAX_URL_BYTES} bytes")]
    UrlTooLong { token: String },
    #[error("tap package '{token}' has an unsafe '{architecture}' artifact URL")]
    UnsafeUrl {
        token: String,
        architecture: String,
        #[source]
        source: UrlError,
    },
    #[error(
        "tap '{tap}' declares artifact URL '{url}' more than once; selected artifacts must be unambiguous"
    )]
    DuplicateUrl { tap: String, url: String },
    #[error(
        "tap '{tap}' declares artifact sha256 '{sha256}' more than once; selected artifacts must be unambiguous"
    )]
    DuplicateSha256 { tap: String, sha256: String },
    #[error(
        "tap '{tap}' has colliding identifiers: package '{token}' and executable '{bin}' name different packages"
    )]
    CollidingIdentifiers {
        tap: String,
        token: String,
        bin: String,
    },
    #[error("tap '{tap}' does not declare package '{token}'; declared packages: {declared}")]
    UndeclaredPackage {
        tap: String,
        token: String,
        declared: String,
    },
    #[error(
        "tap package '{token}' has no {architecture} artifact; declared architectures: {declared}"
    )]
    NoArtifactForArchitecture {
        token: String,
        architecture: String,
        declared: String,
    },
}

/// Why a tap registry record or tap receipt was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error("the tap manifest path must be valid UTF-8")]
    ManifestPathNotUtf8,
    #[error("the tap source manifest path must be valid UTF-8")]
    SourceManifestPathNotUtf8,
    #[error("unsupported tap registry schema {found}; expected {SCHEMA_VERSION}")]
    UnsupportedRegistrySchema { found: u32 },
    #[error("tap registry records '{recorded}', not '{expected}'")]
    RegistryTapMismatch { recorded: String, expected: String },
    #[error(
        "tap registry has an invalid manifest sha256: expected 64 lowercase hexadecimal characters"
    )]
    InvalidRegistryManifestSha256,
    #[error("unsupported tap receipt schema {found}; expected {SCHEMA_VERSION}")]
    UnsupportedReceiptSchema { found: u32 },
    #[error("tap receipt qualified name '{qualified}' does not match '{tap}' and '{token}'")]
    QualifiedMismatch {
        qualified: String,
        tap: String,
        token: String,
    },
    #[error("tap receipt has unsupported architecture '{architecture}'")]
    UnsupportedArchitecture { architecture: String },
    #[error("tap receipt has an unsafe artifact URL")]
    UnsafeArtifactUrl(#[source] UrlError),
    #[error(
        "tap receipt has an invalid artifact sha256: expected 64 lowercase hexadecimal characters"
    )]
    InvalidArtifactSha256,
    #[error("tap receipt artifact size {bytes} is outside the supported range")]
    ArtifactSize { bytes: u64 },
    #[error(
        "tap receipt has an invalid manifest sha256: expected 64 lowercase hexadecimal characters"
    )]
    InvalidReceiptManifestSha256,
    #[error("tap receipt keg '{found}' is not the expected '{expected}'")]
    KegMismatch { found: String, expected: String },
    #[error("tap receipt link '{found}' is not the expected '{expected}'")]
    LinkMismatch { found: String, expected: String },
    #[error("tap source path must be between 1 and {MAX_SOURCE_PATH_BYTES} bytes")]
    SourcePathLength,
    #[error("tap source path must not contain control characters")]
    SourcePathControl,
    #[error("tap source path '{value}' must be absolute")]
    SourcePathNotAbsolute { value: String },
}
