//! Staged, checksum-verified fetch of cask payloads into `<cache_dir>/native-casks`.
//!
//! Cask artifacts are hosted across upstream vendor CDNs rather than a single
//! registry, so [`validate_cask_url`] accepts public HTTPS origins without
//! credentials, fragments, explicit non-default ports, or loopback/private
//! network targets, and [`fetch`] re-validates every manual redirect hop before
//! connecting. Payloads are hashed while streaming and only promoted into the
//! cache on an exact SHA-256 match.

use std::path::PathBuf;

use reqwest::header::LOCATION;
use reqwest::{Client, Response, StatusCode, Url};

use crate::config::Config;
use crate::domain::validate::{
    is_lower_hex_digest, validate_cask_url, validate_cask_url_with_limit,
};
use crate::infra::cache::NATIVE_CASK_DIR;

mod error;

pub(crate) use self::error::CaskDownloadError;
use super::download::{
    build_verified_client, ensure_isolated_cache_dir, request_timeout, reuse_cached_bottle,
    stage_bottle,
};

/// File extension used for verified cached cask payloads.
const CASK_CACHE_SUFFIX: &str = ".download";
/// Longest accepted redirect target, including signed CDN queries.
const MAX_REDIRECT_URL_BYTES: usize = 8192;
/// Maximum HTTP redirect hops followed for a single cask download.
const MAX_REDIRECTS: usize = 10;

/// Download and SHA-256 verify a cask payload into `<cache_dir>/native-casks`.
///
/// Reuses an already cached `<sha256>.download` regular file after verifying
/// its SHA-256 offline. Corrupt regular files in the cache are discarded and
/// fetched again; symlinks at either the cache directory or cache file path
/// are refused.
pub(crate) async fn fetch(
    config: &Config,
    url: &str,
    sha256: &str,
) -> Result<PathBuf, CaskDownloadError> {
    if !is_lower_hex_digest(sha256) {
        return Err(CaskDownloadError::BadDigest);
    }
    let initial = validate_cask_url(url)?;
    let directory = ensure_isolated_cache_dir(config, NATIVE_CASK_DIR, "native cask")?;
    let destination = directory.join(format!("{sha256}{CASK_CACHE_SUFFIX}"));

    if let Some(cached) = reuse_cached_bottle(&destination, sha256)? {
        return Ok(cached);
    }

    let client = build_verified_client(config, "cask")?;
    let response = request_cask_payload(&client, initial, config).await?;
    stage_bottle(&directory, &destination, sha256, response, config).await?;
    Ok(destination)
}

async fn request_cask_payload(
    client: &Client,
    initial: Url,
    config: &Config,
) -> Result<Response, CaskDownloadError> {
    let mut current = initial;
    let mut redirects = 0_usize;

    loop {
        let response =
            tokio::time::timeout(request_timeout(config), client.get(current.clone()).send())
                .await
                .map_err(|source| CaskDownloadError::RequestTimedOut {
                    url: current.to_string(),
                    source,
                })?
                .map_err(|source| CaskDownloadError::Request {
                    url: current.to_string(),
                    source,
                })?;

        match response.status() {
            StatusCode::OK => return Ok(response),
            StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT => {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(CaskDownloadError::TooManyRedirects);
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(CaskDownloadError::MissingLocation)?;
                current = resolve_cask_redirect(&current, location)?;
            }
            status => {
                return Err(CaskDownloadError::HttpStatus {
                    status,
                    url: current.to_string(),
                });
            }
        }
    }
}

fn resolve_cask_redirect(base: &Url, location: &str) -> Result<Url, CaskDownloadError> {
    if location.is_empty() {
        return Err(CaskDownloadError::EmptyLocation);
    }
    if location.len() > MAX_REDIRECT_URL_BYTES {
        return Err(CaskDownloadError::LocationTooLong);
    }
    if location.chars().any(|ch| ch.is_control()) {
        return Err(CaskDownloadError::LocationControlCharacters);
    }
    let target = base
        .join(location)
        .map_err(|source| CaskDownloadError::InvalidLocation {
            location: location.to_owned(),
            source,
        })?;
    Ok(validate_cask_url_with_limit(
        target.as_str(),
        MAX_REDIRECT_URL_BYTES,
        "cask redirect URL",
    )?)
}

#[cfg(test)]
mod tests;
