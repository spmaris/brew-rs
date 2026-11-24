//! Typed failures of the cask payload request.
//!
//! `Display` texts match the former `anyhow` messages exactly.

use reqwest::StatusCode;
use tokio::time::error::Elapsed;

use super::{MAX_REDIRECT_URL_BYTES, MAX_REDIRECTS};
use crate::domain::validate::UrlError;
use crate::infra::download::DownloadError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CaskDownloadError {
    #[error(transparent)]
    Url(#[from] UrlError),
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error("refusing to fetch cask payload: sha256 must be 64 lowercase hexadecimal characters")]
    BadDigest,
    #[error("timed out requesting cask payload from {url}")]
    RequestTimedOut { url: String, source: Elapsed },
    #[error("failed to request cask payload from {url}")]
    Request { url: String, source: reqwest::Error },
    #[error("refusing to follow more than {MAX_REDIRECTS} cask redirects")]
    TooManyRedirects,
    #[error("cask download redirect did not include a valid Location header")]
    MissingLocation,
    #[error("cask download failed with HTTP {status} from {url}")]
    HttpStatus { status: StatusCode, url: String },
    #[error("refusing empty cask redirect Location header")]
    EmptyLocation,
    #[error("refusing cask redirect: Location header exceeds {MAX_REDIRECT_URL_BYTES} bytes")]
    LocationTooLong,
    #[error("refusing cask redirect: Location header contains control characters")]
    LocationControlCharacters,
    #[error("cask redirect has an invalid Location header: {location}")]
    InvalidLocation {
        location: String,
        source: url::ParseError,
    },
}
