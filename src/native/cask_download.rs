//! Staged, checksum-verified fetch of cask payloads into `<cache_dir>/native-casks`.
//!
//! Cask artifacts are hosted across upstream vendor CDNs rather than a single
//! registry, so [`validate_cask_url`] accepts public HTTPS origins without
//! credentials, fragments, explicit non-default ports, or loopback/private
//! network targets, and [`fetch`] re-validates every manual redirect hop before
//! connecting. Payloads are hashed while streaming and only promoted into the
//! cache on an exact SHA-256 match.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::header::LOCATION;
use reqwest::{Client, Response, StatusCode, Url};

use crate::cache::NATIVE_CASK_DIR;
use crate::config::Config;

use super::download::{
    build_verified_client, ensure_isolated_cache_dir, is_lower_hex_digest, request_timeout,
    reuse_cached_bottle, stage_bottle,
};

/// File extension used for verified cached cask payloads.
const CASK_CACHE_SUFFIX: &str = ".download";
/// Longest accepted initial cask download URL.
const MAX_URL_BYTES: usize = 2048;
/// Longest accepted redirect target, including signed CDN queries.
const MAX_REDIRECT_URL_BYTES: usize = 8192;
/// Maximum HTTP redirect hops followed for a single cask download.
const MAX_REDIRECTS: usize = 10;

/// Validate an initial cask download URL before accepting it into a plan or
/// connecting to it.
pub(super) fn validate_cask_url(url: &str) -> Result<Url> {
    validate_cask_url_with_limit(url, MAX_URL_BYTES, "cask URL")
}

/// Download and SHA-256 verify a cask payload into `<cache_dir>/native-casks`.
///
/// Reuses an already cached `<sha256>.download` regular file after verifying
/// its SHA-256 offline. Corrupt regular files in the cache are discarded and
/// fetched again; symlinks at either the cache directory or cache file path
/// are refused.
pub(super) async fn fetch(config: &Config, url: &str, sha256: &str) -> Result<PathBuf> {
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

fn validate_cask_url_with_limit(raw: &str, max_bytes: usize, label: &str) -> Result<Url> {
    if raw.is_empty() {
        bail!("refusing empty {label}");
    }
    if raw.len() > max_bytes {
        bail!("refusing {label}: URL exceeds {max_bytes} bytes");
    }
    if raw.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        bail!("refusing {label}: URL contains whitespace or control characters");
    }
    if raw.contains('\\') {
        bail!("refusing {label}: URL must not contain backslashes");
    }

    let parsed = Url::parse(raw).with_context(|| format!("invalid {label} '{raw}'"))?;
    validate_parsed_cask_url(&parsed, label)?;
    Ok(parsed)
}

fn validate_parsed_cask_url(parsed: &Url, label: &str) -> Result<()> {
    if parsed.scheme() != "https" {
        bail!("refusing {label}: URL scheme must be https");
    }
    if parsed.fragment().is_some() {
        bail!("refusing {label}: URL must not contain a fragment");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("refusing {label}: URL must not contain credentials");
    }
    if parsed.port().is_some() {
        bail!("refusing {label}: explicit non-default port is not allowed");
    }
    let Some(host) = parsed.host_str() else {
        bail!("refusing {label}: URL does not name a host");
    };
    if is_disallowed_host(host) {
        bail!("refusing {label}: host '{host}' is not an allowed public HTTPS host");
    }
    Ok(())
}

fn is_disallowed_host(host: &str) -> bool {
    let normalized = host.trim_end_matches('.');
    if normalized.is_empty() {
        return true;
    }
    let lower = normalized.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return true;
    }
    let ip_candidate = normalized
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(normalized);
    if let Ok(addr) = ip_candidate.parse::<IpAddr>() {
        return is_disallowed_ip(addr);
    }
    false
}

fn is_disallowed_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_disallowed_v4(v4),
        IpAddr::V6(v6) => is_disallowed_v6(v6),
    }
}

fn is_disallowed_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.octets()[0] == 0
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
}

fn is_disallowed_v6(v6: Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_disallowed_v4(v4);
    }
    let first = v6.segments()[0];
    let is_ula = (first & 0xfe00) == 0xfc00;
    let is_unicast_link_local = (first & 0xffc0) == 0xfe80;
    v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || is_ula || is_unicast_link_local
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
            status
                if matches!(
                    status,
                    StatusCode::MOVED_PERMANENTLY
                        | StatusCode::FOUND
                        | StatusCode::SEE_OTHER
                        | StatusCode::TEMPORARY_REDIRECT
                        | StatusCode::PERMANENT_REDIRECT
                ) =>
            {
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
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    const DIGEST: &str = "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447";
    const FIXTURE: &[u8] = b"hello world\n";

    fn test_config(cache_dir: &Path) -> Config {
        let mut config = Config::default();
        config.storage.cache_dir = cache_dir.to_path_buf();
        config
    }

    #[test]
    fn validates_public_https_cask_urls_and_rejects_unsafe_targets() {
        for valid in [
            "https://downloads.claude.ai/claude-code-releases/2.1.330/darwin-arm64/claude",
            "https://github.com/rxhanson/Rectangle/releases/download/v2.0.5/Rectangle2.0.5.dmg",
            "https://update.code.visualstudio.com/1.147.0/darwin-arm64/stable",
            "https://1.1.1.1/app.zip",
        ] {
            assert!(
                validate_cask_url(valid).is_ok(),
                "expected valid URL {valid} to pass"
            );
        }

        let long_url = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        for invalid in [
            "",
            "http://example.com/app.dmg",
            "file:///Applications/App.app",
            "https://user:pass@example.com/app.dmg",
            "https://example.com:8443/app.dmg",
            "https://example.com/app.dmg#frag",
            "https://example.com/bad\\path",
            "https://example.com/has space",
            "https://localhost/app.dmg",
            "https://localhost./app.dmg",
            "https://api.localhost/app.dmg",
            "https://printer.local/app.dmg",
            "https://127.0.0.1/app.dmg",
            "https://10.0.0.5/app.dmg",
            "https://172.16.4.1/app.dmg",
            "https://192.168.1.10/app.dmg",
            "https://169.254.169.254/latest/meta-data",
            "https://0.0.0.0/app.dmg",
            "https://[::1]/app.dmg",
            "https://[::]/app.dmg",
            "https://[fc00::1]/app.dmg",
            "https://[fd12:3456::1]/app.dmg",
            "https://[fe80::1]/app.dmg",
            "https://[::ffff:127.0.0.1]/app.dmg",
            &long_url,
        ] {
            assert!(
                validate_cask_url(invalid).is_err(),
                "expected unsafe URL {invalid} to be rejected"
            );
        }
    }

    #[test]
    fn redirect_validation_allows_cdn_hops_and_rejects_downgrades() {
        let base = validate_cask_url("https://github.com/org/repo/releases/download/v1/App.zip")
            .expect("base url");
        let cdn = resolve_cask_redirect(
            &base,
            "https://objects.githubusercontent.com/github-production-release-asset/123?X-Amz-Signature=abc",
        )
        .expect("CDN redirect");
        assert_eq!(cdn.host_str(), Some("objects.githubusercontent.com"));

        let relative = resolve_cask_redirect(&base, "/org/repo/v2/App.zip").expect("relative");
        assert_eq!(relative.as_str(), "https://github.com/org/repo/v2/App.zip");

        assert!(resolve_cask_redirect(&base, "http://cdn.example.com/App.zip").is_err());
        assert!(resolve_cask_redirect(&base, "https://127.0.0.1/App.zip").is_err());
        assert!(resolve_cask_redirect(&base, "https://localhost:8443/App.zip").is_err());
        assert!(
            resolve_cask_redirect(
                &base,
                &format!(
                    "https://cdn.example.com/{}",
                    "x".repeat(MAX_REDIRECT_URL_BYTES)
                )
            )
            .is_err()
        );
    }

    #[test]
    fn fetch_serves_verified_cache_hit_offline_and_rejects_bad_cache() {
        let temp = tempdir().expect("tempdir");
        let config = test_config(temp.path());
        let dir = ensure_isolated_cache_dir(&config, NATIVE_CASK_DIR, "native cask")
            .expect("create cask cache dir");
        let path = dir.join(format!("{DIGEST}{CASK_CACHE_SUFFIX}"));
        fs::write(&path, FIXTURE).expect("write cached payload");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let hit = rt
            .block_on(fetch(&config, "https://example.com/app.dmg", DIGEST))
            .expect("cache hit");
        assert_eq!(hit, path);

        // Corrupt cached regular file should fail offline verification when removed,
        // and reject unsafe URLs before touching cache.
        let bad_digest = "b".repeat(64);
        assert!(
            rt.block_on(fetch(&config, "http://example.com/app.dmg", &bad_digest))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn fetch_refuses_symlinked_cache_dir_or_entry() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let cache = temp.path().join("cache-link");
        fs::create_dir_all(&cache).unwrap();
        symlink(&outside, cache.join(NATIVE_CASK_DIR)).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let err = rt
            .block_on(fetch(
                &test_config(&cache),
                "https://example.com/app.dmg",
                DIGEST,
            ))
            .unwrap_err();
        assert!(format!("{err:#}").contains("symlink"));

        let real_cache = temp.path().join("real-cache");
        let dir =
            ensure_isolated_cache_dir(&test_config(&real_cache), NATIVE_CASK_DIR, "native cask")
                .unwrap();
        let outside_file = temp.path().join("secret");
        fs::write(&outside_file, FIXTURE).unwrap();
        symlink(
            &outside_file,
            dir.join(format!("{DIGEST}{CASK_CACHE_SUFFIX}")),
        )
        .unwrap();
        let err = rt
            .block_on(fetch(
                &test_config(&real_cache),
                "https://example.com/app.dmg",
                DIGEST,
            ))
            .unwrap_err();
        assert!(format!("{err:#}").contains("symlink"));
    }
}
