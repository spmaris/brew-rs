//! Staged, checksum-verified fetch of official Homebrew bottles from GHCR.
//!
//! [`fetch`] is deliberately narrow. It only accepts lowercase SHA-256 digests
//! and `https://ghcr.io/v2/homebrew/core/.../blobs/sha256:<digest>` URLs, and it
//! only ever writes into `<storage.cache_dir>/native-bottles`. Archives are
//! streamed to a private staging file, hashed while they are written, verified
//! against the expected digest, and published with an atomic rename. Nothing is
//! unpacked, executed, or registered with Homebrew, and no global state is
//! touched.
//!
//! Cached files are re-verified before every reuse: a corrupt or incomplete
//! regular file is discarded and fetched again, while anything that is not a
//! regular file (notably a symlink) is refused instead of followed.

mod cache;
mod consts;
mod error;
mod request;
mod stage;
mod url;

use std::path::PathBuf;

use crate::config::Config;

pub(super) use cache::{ensure_isolated_cache_dir, reuse_cached_bottle, verify_file_digest};
pub(crate) use error::DownloadError;
pub(super) use request::{build_verified_client, request_timeout};
pub(super) use stage::stage_bottle;

use cache::bottle_cache_dir;
use consts::BOTTLE_SUFFIX;
use request::{build_client, request_bottle};
use url::parse_bottle_url;

/// Fetch an official Homebrew bottle into the native bottle cache.
///
/// The archive is written atomically to
/// `<storage.cache_dir>/native-bottles/<sha256>.tar.gz` after its digest has
/// been verified while streaming. A previously cached file is reused only when
/// it still matches `sha256`; a mismatched regular file is discarded and
/// downloaded again. Nothing is unpacked or installed.
///
/// `url` must be an `https://ghcr.io/v2/homebrew/core/.../blobs/sha256:<digest>`
/// URL with no credentials, port, query, fragment, percent-encoding, or path
/// traversal segments, and `sha256` must be exactly 64 lowercase hexadecimal
/// characters equal to the digest embedded in `url`.
pub async fn fetch(config: &Config, url: &str, sha256: &str) -> Result<PathBuf, DownloadError> {
    let source = parse_bottle_url(url, sha256)?;
    let directory = bottle_cache_dir(config)?;
    let destination = directory.join(format!("{}{BOTTLE_SUFFIX}", source.digest));

    // Validate the cache entry before creating a client so a verified file can
    // be served entirely offline.
    if let Some(cached) = reuse_cached_bottle(&destination, &source.digest)? {
        return Ok(cached);
    }

    let client = build_client(config)?;
    let response = request_bottle(&client, &source, config).await?;
    stage_bottle(&directory, &destination, &source.digest, response, config).await?;
    Ok(destination)
}

#[cfg(test)]
mod tests;
