//! Staged, checksum-verified fetch of cask payloads into `<cache_dir>/native-casks`.
//!
//! Cask artifacts are hosted across upstream vendor CDNs rather than a single
//! registry, so [`validate_cask_url`] accepts public HTTPS origins without
//! credentials, fragments, explicit non-default ports, or loopback/private
//! network targets, and [`fetch`] re-validates every manual redirect hop before
//! connecting. Payloads are hashed while streaming and only promoted into the
//! cache on an exact SHA-256 match.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::header::LOCATION;
use reqwest::{Client, Response, StatusCode, Url};

use crate::config::Config;
use crate::domain::validate::{
    is_lower_hex_digest, validate_cask_url, validate_cask_url_with_limit,
};
use crate::infra::cache::NATIVE_CASK_DIR;

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
pub(crate) async fn fetch(config: &Config, url: &str, sha256: &str) -> Result<PathBuf> {
    if !is_lower_hex_digest(sha256) {
        bail!("refusing to fetch cask payload: sha256 must be 64 lowercase hexadecimal characters");
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

async fn request_cask_payload(client: &Client, initial: Url, config: &Config) -> Result<Response> {
    let mut current = initial;
    let mut redirects = 0_usize;

    loop {
        let response =
            tokio::time::timeout(request_timeout(config), client.get(current.clone()).send())
                .await
                .with_context(|| format!("timed out requesting cask payload from {current}"))?
                .with_context(|| format!("failed to request cask payload from {current}"))?;

        match response.status() {
            StatusCode::OK => return Ok(response),
            StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT => {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    bail!("refusing to follow more than {MAX_REDIRECTS} cask redirects");
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .context("cask download redirect did not include a valid Location header")?;
                current = resolve_cask_redirect(&current, location)?;
            }
            status => bail!("cask download failed with HTTP {status} from {current}"),
        }
    }
}

fn resolve_cask_redirect(base: &Url, location: &str) -> Result<Url> {
    if location.is_empty() {
        bail!("refusing empty cask redirect Location header");
    }
    if location.len() > MAX_REDIRECT_URL_BYTES {
        bail!("refusing cask redirect: Location header exceeds {MAX_REDIRECT_URL_BYTES} bytes");
    }
    if location.chars().any(|ch| ch.is_control()) {
        bail!("refusing cask redirect: Location header contains control characters");
    }
    let target = base
        .join(location)
        .with_context(|| format!("cask redirect has an invalid Location header: {location}"))?;
    validate_cask_url_with_limit(target.as_str(), MAX_REDIRECT_URL_BYTES, "cask redirect URL")
}

#[cfg(test)]
mod tests;
