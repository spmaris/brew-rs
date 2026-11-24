//! Typed failures of the official API client and its catalog cache.
//!
//! `Display` texts match the former `anyhow` context messages exactly so the
//! `{:#}` chains users and logs see are unchanged.

use std::error::Error as StdError;
use std::io;
use std::path::PathBuf;

use reqwest::StatusCode;
use tokio::time::error::Elapsed;

use crate::domain::plan_error::ApiFailure;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiError {
    #[error("Failed to create HTTP client")]
    ClientBuild(#[source] reqwest::Error),
    #[error("Request timed out")]
    TimedOut(#[source] Elapsed),
    #[error("Failed to send request")]
    Send(#[source] reqwest::Error),
    /// The server answered with an HTTP error status; the status and URL live
    /// on the wrapped `reqwest::Error`.
    #[error("API request failed")]
    Status(#[source] reqwest::Error),
    #[error("Unexpected API response: HTTP {0}")]
    UnexpectedStatus(StatusCode),
    #[error("Repository JSON exceeds the 96 MiB response limit")]
    ResponseTooLarge,
    #[error("Failed to read API response")]
    ReadBody(#[source] reqwest::Error),
    #[error("Failed to parse JSON response")]
    InvalidJson(#[source] serde_json::Error),

    #[error("Failed to fetch {catalog} from the Homebrew API")]
    CatalogFetch {
        catalog: &'static str,
        source: Box<ApiError>,
    },
    #[error("failed to fetch formula '{name}'")]
    FormulaFetch { name: String, source: Box<ApiError> },
    #[error("failed to fetch formula '{name}' from {url}")]
    FormulaFetchFrom {
        name: String,
        url: String,
        source: Box<ApiError>,
    },
    #[error("failed to fetch formula '{canonical}' (resolved from alias '{alias}')")]
    FormulaAliasFetch {
        canonical: String,
        alias: String,
        source: Box<ApiError>,
    },
    #[error("failed to fetch cask '{token}' from {url}")]
    CaskFetch {
        token: String,
        url: String,
        source: Box<ApiError>,
    },
    #[error("failed to fetch cask '{canonical}' (resolved from '{token}') from {url}")]
    CaskAliasFetch {
        canonical: String,
        token: String,
        url: String,
        source: Box<ApiError>,
    },

    #[error("Refusing to clean catalog cache through symlinked directory {}", path.display())]
    CacheDirSymlink { path: PathBuf },
    #[error("Catalog cache path {} is not a directory", path.display())]
    CacheDirNotDirectory { path: PathBuf },
    #[error("Failed to inspect catalog cache {}", path.display())]
    InspectCacheDir { path: PathBuf, source: io::Error },
    #[error("Failed to inspect cache file {}", path.display())]
    InspectCacheFile { path: PathBuf, source: io::Error },
    #[error("Failed to remove cache file {}", path.display())]
    RemoveCacheFile { path: PathBuf, source: io::Error },
    #[error("No cached catalog at {}; run a search without --offline once to populate it", path.display())]
    NoCachedCatalog { path: PathBuf },
    #[error("Failed to read {}", path.display())]
    ReadCache { path: PathBuf, source: io::Error },
    #[error("Failed to read the timestamp of {}", path.display())]
    CacheTimestamp { path: PathBuf, source: io::Error },
    #[error("Failed to parse {}", path.display())]
    ParseCache {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("Catalog cache path has no parent directory")]
    CachePathHasNoParent,
    #[error("Failed to create catalog cache directory {}", path.display())]
    CreateCacheDir { path: PathBuf, source: io::Error },
    #[error("Failed to serialize catalog cache")]
    SerializeCache(#[source] serde_json::Error),
    #[error("Failed to write {}", path.display())]
    WriteCache { path: PathBuf, source: io::Error },
    #[error("Failed to replace {}", path.display())]
    ReplaceCache { path: PathBuf, source: io::Error },
}

impl ApiError {
    /// How this failure should be presented to the user, if it is a
    /// not-found, unreachable or 5xx failure.
    pub(crate) fn failure(&self) -> Option<ApiFailure> {
        std::iter::successors(Some(self as &(dyn StdError + 'static)), |&cause| {
            cause.source()
        })
        .find_map(classify_cause)
    }

    /// Whether the chain carries a typed HTTP `404 Not Found` status.
    pub(crate) fn is_not_found(&self) -> bool {
        std::iter::successors(Some(self as &(dyn StdError + 'static)), |&cause| {
            cause.source()
        })
        .any(is_not_found_cause)
    }
}

pub(super) fn is_not_found_cause(cause: &(dyn StdError + 'static)) -> bool {
    cause
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        == Some(StatusCode::NOT_FOUND)
}

pub(super) fn classify_cause(cause: &(dyn StdError + 'static)) -> Option<ApiFailure> {
    if let Some(failure) = cause.downcast_ref::<ApiFailure>() {
        return Some(*failure);
    }
    if cause.is::<Elapsed>() {
        return Some(ApiFailure::Unreachable);
    }
    let error = cause.downcast_ref::<reqwest::Error>()?;
    classify_parts(
        error.status().map(|status| status.as_u16()),
        error.is_connect() || error.is_timeout() || error.is_request(),
    )
}

pub(super) fn classify_parts(status: Option<u16>, transport_failure: bool) -> Option<ApiFailure> {
    match status {
        Some(404) => Some(ApiFailure::NotFound),
        Some(code) if (500..600).contains(&code) => Some(ApiFailure::Server(code)),
        Some(_) => None,
        None => transport_failure.then_some(ApiFailure::Unreachable),
    }
}

/// `error` and its causes joined with `": "`, the same text `anyhow` prints
/// for `{:#}`.
pub(crate) fn error_chain(error: &(dyn StdError + 'static)) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    text
}
