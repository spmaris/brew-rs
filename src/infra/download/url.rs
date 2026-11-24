use crate::domain::validate::is_lower_hex_digest;

use super::consts::{
    BOTTLE_PATH_PREFIX, GHCR_HOST, GHCR_ORIGIN, MAX_REDIRECT_URL_BYTES, MAX_REPOSITORY_SEGMENTS,
    MAX_URL_BYTES,
};
use super::error::{DownloadError, UnsafeUrl};

/// A validated bottle location.
pub(super) struct BottleSource {
    /// Registry repository, for example `homebrew/core/wget`.
    pub(super) repository: String,
    /// Lowercase SHA-256 digest from the URL and the caller.
    pub(super) digest: String,
    /// Canonical URL rebuilt from validated components.
    pub(super) url: String,
}

impl BottleSource {
    /// Minimal pull scope needed for exactly this repository.
    pub(super) fn pull_scope(&self) -> String {
        format!("repository:{}:pull", self.repository)
    }
}

/// Parse and strictly validate a GHCR bottle blob URL.
pub(super) fn parse_bottle_url(url: &str, sha256: &str) -> Result<BottleSource, UnsafeUrl> {
    if !is_lower_hex_digest(sha256) {
        return Err(UnsafeUrl::BadDigest);
    }
    if url.len() > MAX_URL_BYTES {
        return Err(UnsafeUrl::TooLong);
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(UnsafeUrl::ControlCharacters);
    }

    // Requiring this exact prefix also rejects ports, userinfo, trailing-dot
    // hosts, and look-alike hosts such as `ghcr.io.evil.example`.
    let Some(path) = url.strip_prefix(GHCR_ORIGIN) else {
        return Err(UnsafeUrl::WrongOrigin {
            origin: GHCR_ORIGIN,
        });
    };
    if !path.starts_with('/') {
        return Err(UnsafeUrl::WrongHost { host: GHCR_HOST });
    }
    if path.contains('?') || path.contains('#') {
        return Err(UnsafeUrl::QueryOrFragment);
    }
    if path.contains('%') {
        return Err(UnsafeUrl::PercentEncoding);
    }
    if path.contains('\\') {
        return Err(UnsafeUrl::Backslash);
    }

    let Some(rest) = path.strip_prefix(BOTTLE_PATH_PREFIX) else {
        return Err(UnsafeUrl::WrongPrefix {
            prefix: BOTTLE_PATH_PREFIX,
        });
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.len() < 3 {
        return Err(UnsafeUrl::TooFewSegments);
    }

    let digest_segment = segments[segments.len() - 1];
    let blob_segment = segments[segments.len() - 2];
    if blob_segment != "blobs" {
        return Err(UnsafeUrl::NotBlobPath);
    }
    let Some(url_digest) = digest_segment.strip_prefix("sha256:") else {
        return Err(UnsafeUrl::NoDigestSegment);
    };
    if url_digest != sha256 {
        return Err(UnsafeUrl::DigestMismatch);
    }

    let repository_segments = &segments[..segments.len() - 2];
    if repository_segments.is_empty() || repository_segments.len() > MAX_REPOSITORY_SEGMENTS {
        return Err(UnsafeUrl::RepositoryDepth);
    }
    for segment in repository_segments {
        if !is_valid_repository_segment(segment) {
            return Err(UnsafeUrl::InvalidSegment {
                segment: (*segment).to_owned(),
            });
        }
    }

    let digest = sha256.to_owned();
    let repository_path = repository_segments.join("/");
    Ok(BottleSource {
        repository: format!("homebrew/core/{repository_path}"),
        url: format!("{GHCR_ORIGIN}{BOTTLE_PATH_PREFIX}{repository_path}/blobs/sha256:{digest}"),
        digest,
    })
}

/// Whether `segment` is one safe, lowercase registry path component.
pub(super) fn is_valid_repository_segment(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > 64 || segment == "." || segment == ".." {
        return false;
    }
    let mut characters = segment.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-' | '+' | '@')
    })
}

/// Whether `url` targets the GHCR origin.
pub(super) fn is_ghcr_url(url: &str) -> bool {
    url.starts_with("https://ghcr.io/")
}

/// Resolve and validate one redirect hop.
pub(super) fn resolve_redirect(base: &str, location: &str) -> Result<String, DownloadError> {
    if location.len() > MAX_REDIRECT_URL_BYTES {
        return Err(DownloadError::RedirectLocationTooLong);
    }
    let base = reqwest::Url::parse(base).map_err(DownloadError::ParseRequestUrl)?;
    let target = base
        .join(location)
        .map_err(|source| DownloadError::InvalidLocation {
            location: location.to_owned(),
            source,
        })?;
    if target.fragment().is_some() {
        return Err(DownloadError::RedirectFragment);
    }
    if !is_allowed_redirect(&target) {
        return Err(DownloadError::RedirectHostNotAllowed {
            target: target.to_string(),
        });
    }
    Ok(target.to_string())
}

/// Whether a redirect target is an allowed, credential-free GitHub host.
pub(super) fn is_allowed_redirect(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    // An explicit port in the original URL means `Url` keeps it; default ports
    // are normalized away by the parser.
    if url.port().is_some() {
        return false;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    host == GHCR_HOST || host.ends_with(".githubusercontent.com")
}
