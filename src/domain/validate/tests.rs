use super::*;

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
