//! Typed failures of the staged bottle download.
//!
//! `Display` texts match the former `anyhow` messages exactly.

use std::io;
use std::path::PathBuf;

use reqwest::StatusCode;
use tokio::time::error::Elapsed;

use super::consts::{MAX_BOTTLE_BYTES, MAX_REDIRECTS, MAX_TOKEN_BYTES, MAX_URL_BYTES};

#[derive(Debug, thiserror::Error)]
pub(crate) enum DownloadError {
    #[error(transparent)]
    UnsafeUrl(#[from] UnsafeUrl),

    // Cache directory and cached files.
    #[error("refusing to use '{}': the {label} directory is a symlink", path.display())]
    CacheDirSymlink { path: PathBuf, label: &'static str },
    #[error("refusing to use '{}': the {label} path is not a directory", path.display())]
    CacheDirNotDirectory { path: PathBuf, label: &'static str },
    #[error("refusing to use '{}': the {label} directory is not a real directory", path.display())]
    CacheDirNotReal { path: PathBuf, label: &'static str },
    #[error("failed to create the {label} cache directory: {}", path.display())]
    CreateCacheDir {
        path: PathBuf,
        label: &'static str,
        source: io::Error,
    },
    #[error("failed to inspect the {label} cache directory: {}", path.display())]
    InspectCacheDir {
        path: PathBuf,
        label: &'static str,
        source: io::Error,
    },
    #[error("failed to restrict permissions on the {label} cache directory: {}", path.display())]
    RestrictCacheDir {
        path: PathBuf,
        label: &'static str,
        source: io::Error,
    },
    #[error("failed to inspect cached bottle: {}", path.display())]
    InspectCachedBottle { path: PathBuf, source: io::Error },
    #[error("refusing to reuse '{}': the cached bottle is a symlink", path.display())]
    CachedBottleSymlink { path: PathBuf },
    #[error("refusing to reuse '{}': the cached bottle is not a regular file", path.display())]
    CachedBottleNotFile { path: PathBuf },
    #[error("failed to discard an unverified cached bottle: {}", path.display())]
    DiscardCachedBottle { path: PathBuf, source: io::Error },
    #[error("failed to open cached bottle: {}", path.display())]
    OpenCachedBottle { path: PathBuf, source: io::Error },
    #[error("failed to read cached bottle: {}", path.display())]
    ReadCachedBottle { path: PathBuf, source: io::Error },
    #[error("refusing to reuse '{}': the cached bottle is {len} bytes, above the {limit} byte limit", path.display())]
    CachedBottleTooLarge { path: PathBuf, len: u64, limit: u64 },
    #[error("cached bottle size overflowed")]
    CachedBottleSizeOverflow,
    #[error("refusing to reuse '{}': the cached bottle exceeds the {limit} byte limit", path.display())]
    CachedBottleExceedsLimit { path: PathBuf, limit: u64 },
    #[error("SHA-256 mismatch for '{}': expected {expected}, got {actual}", path.display())]
    CachedChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    // HTTP client and requests.
    #[error("failed to build the {label} HTTP client")]
    BuildClient {
        label: &'static str,
        source: reqwest::Error,
    },
    #[error("timed out requesting bottle from {url}")]
    RequestTimedOut { url: String, source: Elapsed },
    #[error("failed to request bottle from {url}")]
    Request { url: String, source: reqwest::Error },
    #[error("GHCR rejected the bottle request with HTTP 401 Unauthorized")]
    Unauthorized,
    #[error("refusing to follow more than {MAX_REDIRECTS} bottle redirects")]
    TooManyRedirects,
    #[error("bottle redirect did not include a valid Location header")]
    MissingLocation,
    #[error("bottle download failed with HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("refusing to follow a bottle redirect: Location header is too long")]
    RedirectLocationTooLong,
    #[error("failed to parse the bottle request URL")]
    ParseRequestUrl(#[source] url::ParseError),
    #[error("bottle redirect has an invalid Location header: {location}")]
    InvalidLocation {
        location: String,
        source: url::ParseError,
    },
    #[error("refusing to follow a bottle redirect with a fragment")]
    RedirectFragment,
    #[error(
        "refusing to follow a bottle redirect to '{target}': not an allowed GitHub container host"
    )]
    RedirectHostNotAllowed { target: String },

    // GHCR pull token.
    #[error("failed to parse the built-in GHCR token endpoint")]
    TokenEndpoint(#[source] url::ParseError),
    #[error("timed out requesting a GHCR pull token")]
    TokenRequestTimedOut(#[source] Elapsed),
    #[error("failed to request a GHCR pull token")]
    TokenRequest(#[source] reqwest::Error),
    #[error("GHCR pull token request failed with HTTP {0}")]
    TokenStatus(StatusCode),
    #[error("timed out reading the GHCR pull token response")]
    TokenReadTimedOut(#[source] Elapsed),
    #[error("failed to read the GHCR pull token response")]
    TokenRead(#[source] reqwest::Error),
    #[error("GHCR pull token response exceeded {MAX_TOKEN_BYTES} bytes")]
    TokenTooLarge,
    #[error("failed to parse the GHCR pull token response")]
    TokenParse(#[source] serde_json::Error),
    #[error("GHCR pull token response contained no token")]
    TokenMissing,
    #[error("GHCR returned an unusable pull token")]
    TokenUnusable,

    // Staging.
    #[error("refusing to download a bottle larger than the {MAX_BOTTLE_BYTES} byte limit")]
    BottleTooLarge,
    #[error("timed out while streaming the bottle")]
    StreamTimedOut(#[source] Elapsed),
    #[error("failed while streaming the bottle")]
    Stream(#[source] reqwest::Error),
    #[error("bottle size overflowed")]
    SizeOverflow,
    #[error("bottle download exceeded the {MAX_BOTTLE_BYTES} byte limit")]
    SizeLimitExceeded,
    #[error("failed to write the staged bottle")]
    WriteStaged(#[source] io::Error),
    #[error("bottle download produced an empty file")]
    EmptyDownload,
    #[error("SHA-256 mismatch for the downloaded bottle: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("failed to create a staging file in {}", directory.display())]
    CreateStagingFile {
        directory: PathBuf,
        source: io::Error,
    },
    #[error("failed to create a unique staging file in {}", directory.display())]
    StagingFileExhausted { directory: PathBuf },
    #[error("failed to flush the staged bottle: {}", path.display())]
    FlushStaged { path: PathBuf, source: io::Error },
    #[error("failed to publish the bottle to {}", destination.display())]
    Publish {
        destination: PathBuf,
        source: io::Error,
    },
    #[error("refusing to replace '{}': the destination is a symlink", path.display())]
    DestinationSymlink { path: PathBuf },
    #[error("failed to inspect '{}'", path.display())]
    InspectDestination { path: PathBuf, source: io::Error },
}

/// Why a bottle URL or digest was refused before any connection was made.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UnsafeUrl {
    #[error("refusing to fetch a bottle: sha256 must be 64 lowercase hexadecimal characters")]
    BadDigest,
    #[error("refusing to fetch a bottle: URL exceeds {MAX_URL_BYTES} bytes")]
    TooLong,
    #[error("refusing to fetch a bottle: URL contains whitespace or control characters")]
    ControlCharacters,
    #[error("refusing to fetch a bottle: URL must start with {origin}/")]
    WrongOrigin { origin: &'static str },
    #[error("refusing to fetch a bottle: URL must be on the {host} origin")]
    WrongHost { host: &'static str },
    #[error("refusing to fetch a bottle: URL must not contain a query or fragment")]
    QueryOrFragment,
    #[error("refusing to fetch a bottle: URL must not contain percent-encoding")]
    PercentEncoding,
    #[error("refusing to fetch a bottle: URL must not contain backslashes")]
    Backslash,
    #[error("refusing to fetch a bottle: expected a {prefix}... path")]
    WrongPrefix { prefix: &'static str },
    #[error("refusing to fetch a bottle: expected <repository>/blobs/sha256:<digest>")]
    TooFewSegments,
    #[error("refusing to fetch a bottle: URL is not a blob download path")]
    NotBlobPath,
    #[error("refusing to fetch a bottle: blob path must end in sha256:<digest>")]
    NoDigestSegment,
    #[error("refusing to fetch a bottle: URL digest does not match the requested sha256")]
    DigestMismatch,
    #[error("refusing to fetch a bottle: unexpected repository path depth")]
    RepositoryDepth,
    #[error("refusing to fetch a bottle: invalid repository segment '{segment}'")]
    InvalidSegment { segment: String },
}
