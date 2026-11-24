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
    let dir = ensure_isolated_cache_dir(&test_config(&real_cache), NATIVE_CASK_DIR, "native cask")
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
