use anyhow::{Context, Result, bail};

use crate::domain::validate::is_lower_hex_digest;

use super::consts::{
    BOTTLE_PATH_PREFIX, GHCR_HOST, GHCR_ORIGIN, MAX_REDIRECT_URL_BYTES, MAX_REPOSITORY_SEGMENTS,
    MAX_URL_BYTES,
};

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
pub(super) fn parse_bottle_url(url: &str, sha256: &str) -> Result<BottleSource> {
    if !is_lower_hex_digest(sha256) {
        bail!("refusing to fetch a bottle: sha256 must be 64 lowercase hexadecimal characters");
    }
    if url.len() > MAX_URL_BYTES {
        bail!("refusing to fetch a bottle: URL exceeds {MAX_URL_BYTES} bytes");
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        bail!("refusing to fetch a bottle: URL contains whitespace or control characters");
    }

    // Requiring this exact prefix also rejects ports, userinfo, trailing-dot
    // hosts, and look-alike hosts such as `ghcr.io.evil.example`.
    let Some(path) = url.strip_prefix(GHCR_ORIGIN) else {
        bail!("refusing to fetch a bottle: URL must start with {GHCR_ORIGIN}/");
    };
    if !path.starts_with('/') {
        bail!("refusing to fetch a bottle: URL must be on the {GHCR_HOST} origin");
    }
    if path.contains('?') || path.contains('#') {
        bail!("refusing to fetch a bottle: URL must not contain a query or fragment");
    }
    if path.contains('%') {
        bail!("refusing to fetch a bottle: URL must not contain percent-encoding");
    }
    if path.contains('\\') {
        bail!("refusing to fetch a bottle: URL must not contain backslashes");
    }

    let Some(rest) = path.strip_prefix(BOTTLE_PATH_PREFIX) else {
        bail!("refusing to fetch a bottle: expected a {BOTTLE_PATH_PREFIX}... path");
    };
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.len() < 3 {
        bail!("refusing to fetch a bottle: expected <repository>/blobs/sha256:<digest>");
    }

    let digest_segment = segments[segments.len() - 1];
    let blob_segment = segments[segments.len() - 2];
    if blob_segment != "blobs" {
        bail!("refusing to fetch a bottle: URL is not a blob download path");
    }
    let Some(url_digest) = digest_segment.strip_prefix("sha256:") else {
        bail!("refusing to fetch a bottle: blob path must end in sha256:<digest>");
    };
    if url_digest != sha256 {
        bail!("refusing to fetch a bottle: URL digest does not match the requested sha256");
    }

    let repository_segments = &segments[..segments.len() - 2];
    if repository_segments.is_empty() || repository_segments.len() > MAX_REPOSITORY_SEGMENTS {
        bail!("refusing to fetch a bottle: unexpected repository path depth");
    }
    for segment in repository_segments {
        if !is_valid_repository_segment(segment) {
            bail!("refusing to fetch a bottle: invalid repository segment '{segment}'");
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
pub(super) fn resolve_redirect(base: &str, location: &str) -> Result<String> {
    if location.len() > MAX_REDIRECT_URL_BYTES {
        bail!("refusing to follow a bottle redirect: Location header is too long");
    }
    let base = reqwest::Url::parse(base).context("failed to parse the bottle request URL")?;
    let target = base
        .join(location)
        .with_context(|| format!("bottle redirect has an invalid Location header: {location}"))?;
    if target.fragment().is_some() {
        bail!("refusing to follow a bottle redirect with a fragment");
    }
    if !is_allowed_redirect(&target) {
        bail!(
            "refusing to follow a bottle redirect to '{}': not an allowed GitHub container host",
            target
        );
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
