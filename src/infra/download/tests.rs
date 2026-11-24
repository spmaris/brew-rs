use super::cache::verify_file_digest_bounded;
use super::consts::BOTTLE_DIR_NAME;
use super::error::UnsafeUrl;
use super::stage::StagedBottle;
use super::url::{is_allowed_redirect, is_ghcr_url, resolve_redirect};
use super::*;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

/// SHA-256 of `FIXTURE`.
const DIGEST: &str = "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447";
/// Offline bottle stand-in; the digest above is of these exact bytes.
const FIXTURE: &[u8] = b"hello world\n";

fn bottle_url(digest: &str) -> String {
    format!("https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:{digest}")
}

fn test_config(cache_dir: &Path) -> Config {
    let mut config = Config::default();
    config.storage.cache_dir = cache_dir.to_path_buf();
    config
}

fn parse(value: &str) -> reqwest::Url {
    reqwest::Url::parse(value).expect("test URL should parse")
}

#[test]
fn accepts_official_bottle_url() {
    let source = parse_bottle_url(&bottle_url(DIGEST), DIGEST).expect("valid bottle URL");
    assert_eq!(source.digest, DIGEST);
    assert_eq!(source.repository, "homebrew/core/wget");
    assert_eq!(source.url, bottle_url(DIGEST));
    assert_eq!(source.pull_scope(), "repository:homebrew/core/wget:pull");
}

#[test]
fn accepts_versioned_repository_paths() {
    let url = format!("https://ghcr.io/v2/homebrew/core/python/3.12/blobs/sha256:{DIGEST}");
    let source = parse_bottle_url(&url, DIGEST).expect("versioned bottle URL");
    assert_eq!(source.repository, "homebrew/core/python/3.12");
    assert_eq!(
        source.pull_scope(),
        "repository:homebrew/core/python/3.12:pull"
    );
}

#[test]
fn rejects_unsafe_or_unrelated_urls() {
    let url = bottle_url(DIGEST);
    let cases = [
        ("http scheme", url.replacen("https://", "http://", 1)),
        (
            "other host",
            url.replacen("ghcr.io", "ghcr.io.evil.example", 1),
        ),
        ("explicit port", url.replacen("ghcr.io", "ghcr.io:443", 1)),
        (
            "userinfo",
            url.replacen("ghcr.io", "user:password@ghcr.io", 1),
        ),
        ("query", format!("{url}?scope=everything")),
        ("fragment", format!("{url}#fragment")),
        (
            "path traversal",
            url.replacen("core/wget", "core/../wget", 1),
        ),
        (
            "encoded traversal",
            url.replacen("core/wget", "core/%2e%2e/wget", 1),
        ),
        ("backslash", url.replacen("core/wget", "core\\wget", 1)),
        ("empty segment", url.replacen("core/wget", "core//wget", 1)),
        (
            "wrong repository prefix",
            url.replacen("/v2/homebrew/core/", "/v2/homebrew/cask/", 1),
        ),
        ("not a blob path", url.replacen("/blobs/", "/manifests/", 1)),
        ("missing digest prefix", url.replacen("sha256:", "", 1)),
        (
            "uppercase digest in URL",
            url.replacen(DIGEST, &DIGEST.to_uppercase(), 1),
        ),
        ("trailing path", format!("{url}/extra")),
        ("whitespace", format!(" {url}")),
        ("empty string", String::new()),
    ];

    for (name, candidate) in cases {
        assert!(
            parse_bottle_url(&candidate, DIGEST).is_err(),
            "unexpectedly accepted {name}: {candidate}"
        );
    }
}

#[test]
fn rejects_invalid_digests() {
    let uppercase = DIGEST.to_uppercase();
    assert!(parse_bottle_url(&bottle_url(&uppercase), &uppercase).is_err());

    let short = &DIGEST[..63];
    let short_url = format!("https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:{short}");
    assert!(parse_bottle_url(&short_url, short).is_err());

    assert!(parse_bottle_url(&bottle_url(DIGEST), &uppercase).is_err());
    assert!(parse_bottle_url(&bottle_url(DIGEST), "").is_err());
    assert!(parse_bottle_url(&bottle_url(DIGEST), &DIGEST.replace('a', "g")).is_err());
}

#[test]
fn rejects_digest_that_differs_from_the_url() {
    let other = "b".repeat(64);
    assert!(parse_bottle_url(&bottle_url(DIGEST), &other).is_err());
}

#[test]
fn redirects_are_limited_to_github_container_hosts() {
    let allowed = [
        "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc",
        "https://pkg-containers.githubusercontent.com/ghcr01/blob/abc?sig=xyz",
        "https://objects.githubusercontent.com/some/object",
    ];
    for candidate in allowed {
        assert!(
            is_allowed_redirect(&parse(candidate)),
            "should allow redirect to {candidate}"
        );
    }

    let denied = [
        "http://pkg-containers.githubusercontent.com/ghcr01/blob/abc",
        "https://evil.example/blob",
        "https://githubusercontent.com/blob",
        "https://githubusercontent.com.evil.example/blob",
        "https://pkg-containers.githubusercontent.com:8443/blob",
        "https://user:pass@pkg-containers.githubusercontent.com/blob",
        "https://127.0.0.1/blob",
        "https://ghcr.io./blob",
    ];
    for candidate in denied {
        assert!(
            !is_allowed_redirect(&parse(candidate)),
            "should refuse redirect to {candidate}"
        );
    }
}

#[test]
fn resolves_relative_redirects_on_ghcr_only() {
    let base = bottle_url(DIGEST);
    let next = resolve_redirect(&base, "/v2/homebrew/core/wget/blobs/sha256:abc")
        .expect("relative redirect on GHCR");
    assert_eq!(
        next,
        "https://ghcr.io/v2/homebrew/core/wget/blobs/sha256:abc"
    );
    assert!(resolve_redirect(&base, "https://evil.example/steal").is_err());
    assert!(resolve_redirect(&base, "http://ghcr.io/v2/steal").is_err());
}

#[test]
fn token_is_only_attached_to_ghcr_urls() {
    assert!(is_ghcr_url("https://ghcr.io/v2/homebrew/core/wget/blobs/x"));
    assert!(!is_ghcr_url("https://ghcr.io.evil.example/v2/x"));
    assert!(!is_ghcr_url("http://ghcr.io/v2/x"));
    assert!(!is_ghcr_url(
        "https://pkg-containers.githubusercontent.com/ghcr01/blob/x"
    ));
}

#[test]
fn creates_private_isolated_bottle_directory() {
    let temp = tempdir().expect("temporary directory");
    let cache_dir = temp.path().join("cache");
    let directory =
        bottle_cache_dir(&test_config(&cache_dir)).expect("create native bottle directory");

    assert_eq!(directory, cache_dir.join(BOTTLE_DIR_NAME));
    assert!(directory.is_dir());
    assert!(
        !fs::symlink_metadata(&directory)
            .expect("directory metadata")
            .file_type()
            .is_symlink()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    // A second call is idempotent.
    assert_eq!(
        bottle_cache_dir(&test_config(&cache_dir)).expect("reuse directory"),
        directory
    );
}

#[test]
fn reuses_only_a_verified_cached_bottle() {
    let temp = tempdir().expect("temporary directory");
    let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
    fs::write(&path, FIXTURE).expect("write fixture");

    assert!(verify_file_digest(&path, DIGEST).is_ok());
    let reused = reuse_cached_bottle(&path, DIGEST).expect("verify cache entry");
    assert_eq!(reused.as_deref(), Some(path.as_path()));
    assert!(path.exists());
}

#[test]
fn discards_a_cached_bottle_that_fails_verification() {
    let temp = tempdir().expect("temporary directory");
    let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
    fs::write(&path, b"corrupt").expect("write corrupt fixture");

    let reused = reuse_cached_bottle(&path, DIGEST).expect("discard corrupt cache entry");
    assert!(reused.is_none());
    assert!(!path.exists(), "an unverified cached file must be removed");
}

#[test]
fn rejects_a_cached_bottle_above_the_size_limit() {
    let temp = tempdir().expect("temporary directory");
    let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
    fs::write(&path, FIXTURE).expect("write fixture");

    let error = verify_file_digest_bounded(&path, DIGEST, FIXTURE.len() as u64 - 1)
        .expect_err("oversized bottle must be refused");
    assert!(matches!(error, DownloadError::CachedBottleTooLarge { .. }));
    assert!(format!("{error:#}").contains("byte limit"));
    assert!(
        path.exists(),
        "a refused file is not removed by verification"
    );
}

#[test]
fn staging_file_removes_itself_unless_committed() {
    let temp = tempdir().expect("temporary directory");
    let staged_path = {
        let mut staged = StagedBottle::create(temp.path(), DIGEST).expect("create staging file");
        staged.write_all(FIXTURE).expect("write staging file");
        assert!(staged.path.exists());
        staged.path.clone()
    };
    assert!(
        !staged_path.exists(),
        "an uncommitted staging file must not survive"
    );
}

#[test]
fn staging_files_have_unique_names() {
    let temp = tempdir().expect("temporary directory");
    let first = StagedBottle::create(temp.path(), DIGEST).expect("first staging file");
    let second = StagedBottle::create(temp.path(), DIGEST).expect("second staging file");

    assert_ne!(first.path, second.path);
    assert!(first.path.exists() && second.path.exists());
}

#[test]
fn commit_publishes_the_verified_bottle() {
    let temp = tempdir().expect("temporary directory");
    let destination = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
    fs::write(&destination, b"stale").expect("write old file");

    let mut staged = StagedBottle::create(temp.path(), DIGEST).expect("create staging file");
    staged.write_all(FIXTURE).expect("write staging file");
    staged.commit(&destination).expect("publish bottle");

    assert_eq!(fs::read(&destination).expect("read bottle"), FIXTURE);
    assert!(verify_file_digest(&destination, DIGEST).is_ok());
}

#[test]
fn fetch_rejects_bad_urls_before_touching_the_cache() {
    let temp = tempdir().expect("temporary directory");
    let config = test_config(temp.path());

    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(fetch(
            &config,
            "https://example.com/v2/homebrew/core/wget/blobs/sha256/x",
            DIGEST,
        ))
        .expect_err("non-GHCR URL must be refused");

    assert!(matches!(
        error,
        DownloadError::UnsafeUrl(UnsafeUrl::WrongOrigin { .. })
    ));
    assert!(format!("{error:#}").contains("ghcr.io"));
    assert!(
        !temp.path().join(BOTTLE_DIR_NAME).exists(),
        "URL validation must run before the cache is created"
    );
}

#[test]
fn fetch_serves_a_verified_cache_hit_offline() {
    let temp = tempdir().expect("temporary directory");
    let config = test_config(temp.path());
    let directory = bottle_cache_dir(&config).expect("create cache directory");
    let path = directory.join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
    fs::write(&path, FIXTURE).expect("write fixture");

    let fetched = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(fetch(&config, &bottle_url(DIGEST), DIGEST))
        .expect("cache hit");

    assert_eq!(fetched, path);
}

#[cfg(unix)]
#[test]
fn refuses_a_symlinked_bottle_directory() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().expect("temporary directory");
    let cache_dir = temp.path().join("cache");
    fs::create_dir_all(&cache_dir).expect("create cache directory");
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside directory");
    symlink(&outside, cache_dir.join(BOTTLE_DIR_NAME)).expect("symlink bottle directory");

    assert!(
        bottle_cache_dir(&test_config(&cache_dir)).is_err(),
        "a symlinked native bottle directory must be refused"
    );
}

#[cfg(unix)]
#[test]
fn refuses_a_symlinked_cached_bottle() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().expect("temporary directory");
    let outside = temp.path().join("outside.tar.gz");
    fs::write(&outside, FIXTURE).expect("write outside fixture");
    let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
    symlink(&outside, &path).expect("symlink cache entry");

    assert!(
        matches!(
            reuse_cached_bottle(&path, DIGEST),
            Err(DownloadError::CachedBottleSymlink { .. })
        ),
        "a symlinked cached bottle must be refused, not followed"
    );
    assert!(outside.exists(), "the symlink target must be untouched");
}

#[test]
fn checksum_mismatch_reports_expected_and_actual_digests() {
    let temp = tempdir().expect("temporary directory");
    let path = temp.path().join(format!("{DIGEST}{BOTTLE_SUFFIX}"));
    fs::write(&path, b"corrupt").expect("write corrupt fixture");

    let error = verify_file_digest(&path, DIGEST).expect_err("corrupt bottle must not verify");
    let DownloadError::CachedChecksumMismatch {
        path: reported,
        expected,
        actual,
    } = &error
    else {
        panic!("expected a checksum mismatch, got {error:?}");
    };
    assert_eq!(reported, &path);
    assert_eq!(expected, DIGEST);
    assert_ne!(actual, DIGEST);
    assert_eq!(
        error.to_string(),
        format!(
            "SHA-256 mismatch for '{}': expected {DIGEST}, got {actual}",
            path.display()
        )
    );
}
