use super::*;

const AGENT_TUI: &str = r#"schema = 1
tap = "pproenca/tap"

[[packages]]
token = "agent-tui"
version = "1.1.0"
bin = "agent-tui"

[packages.artifacts.arm64]
url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64"
sha256 = "0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5"

[packages.artifacts.x86_64]
url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-x64"
sha256 = "81c071afb4df3c688059ebe6336f543cfe5e79082147d3571274c2508ea798cb"
"#;

fn single_artifact_manifest(architecture: &str, url: &str, sha256: &str) -> String {
    format!(
        r#"schema = 1
tap = "owner/tap"

[[packages]]
token = "tool"
version = "1.0"
bin = "tool"

[packages.artifacts.{architecture}]
url = "{url}"
sha256 = "{sha256}"
"#
    )
}

#[test]
fn upstream_agent_tui_manifest_is_supported_as_data() {
    // Transcribed from the user's `pproenca/tap` formula. The formula's
    // Ruby is never read or evaluated by this lane.
    let manifest = TapManifest::parse(AGENT_TUI.as_bytes(), "pproenca/tap").unwrap();
    assert_eq!(manifest.schema, 1);
    assert_eq!(manifest.packages.len(), 1);

    let package = manifest.package("agent-tui").unwrap();
    assert_eq!(package.version, "1.1.0");
    assert_eq!(package.bin, "agent-tui");
    assert_eq!(
        package.artifact_for("arm64").unwrap().sha256,
        "0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5"
    );
    assert_eq!(
        package.artifact_for("x86_64").unwrap().sha256,
        "81c071afb4df3c688059ebe6336f543cfe5e79082147d3571274c2508ea798cb"
    );
    assert!(package.artifact_for("aarch64").is_err());
    assert!(manifest.package("missing").is_err());
}

#[test]
fn qualified_targets_require_three_components() {
    let target = parse_qualified("pproenca/tap/agent-tui").unwrap();
    assert_eq!(target.tap(), "pproenca/tap");
    assert_eq!(target.token, "agent-tui");

    for invalid in [
        "agent-tui",
        "pproenca/agent-tui",
        "pproenca/tap/agent-tui/extra",
        "PPROENCA/tap/agent-tui",
        "pproenca//agent-tui",
        "homebrew/core/wget",
        "user/core/tool",
        "user/tap/../tool",
        "user/tap/",
    ] {
        assert!(
            parse_qualified(invalid).is_err(),
            "expected invalid target {invalid} to be rejected"
        );
    }
}

#[test]
fn tap_names_reject_reserved_and_unsafe_components() {
    assert_eq!(
        parse_tap_name("pproenca/tap").unwrap(),
        ("pproenca".into(), "tap".into())
    );
    for invalid in [
        "homebrew/core",
        "linuxbrew/core",
        "user/core",
        "user/cask",
        "user/tap/extra",
        "user",
        "../tap",
        "user/../tap",
        "User/tap",
        "user/.tap",
        "user/tap.",
    ] {
        assert!(
            parse_tap_name(invalid).is_err(),
            "expected invalid tap {invalid} to be rejected"
        );
    }
}

#[test]
fn manifest_rejects_unsupported_schema_and_unknown_fields() {
    let wrong_schema = AGENT_TUI.replace("schema = 1", "schema = 2");
    assert!(TapManifest::parse(wrong_schema.as_bytes(), "pproenca/tap").is_err());

    let unknown = format!("{AGENT_TUI}\nunexpected = true\n");
    assert!(TapManifest::parse(unknown.as_bytes(), "pproenca/tap").is_err());

    let wrong_tap = AGENT_TUI.replace("pproenca/tap", "other/tap");
    assert!(TapManifest::parse(wrong_tap.as_bytes(), "pproenca/tap").is_err());
}

#[test]
fn manifest_rejects_duplicate_and_colliding_identifiers() {
    let duplicate_token = format!(
        r#"{AGENT_TUI}
[[packages]]
token = "agent-tui"
version = "2.0"
bin = "other"

[packages.artifacts.arm64]
url = "https://example.com/other"
sha256 = "{}"
"#,
        "a".repeat(64)
    );
    assert!(TapManifest::parse(duplicate_token.as_bytes(), "pproenca/tap").is_err());

    let duplicate_bin = format!(
        r#"{AGENT_TUI}
[[packages]]
token = "other"
version = "2.0"
bin = "agent-tui"

[packages.artifacts.arm64]
url = "https://example.com/other"
sha256 = "{}"
"#,
        "a".repeat(64)
    );
    assert!(TapManifest::parse(duplicate_bin.as_bytes(), "pproenca/tap").is_err());
}

#[test]
fn manifest_rejects_ambiguous_artifacts() {
    let duplicate_url = format!(
        r#"{AGENT_TUI}
[[packages]]
token = "other"
version = "2.0"
bin = "other"

[packages.artifacts.arm64]
url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64"
sha256 = "{}"
"#,
        "b".repeat(64)
    );
    assert!(TapManifest::parse(duplicate_url.as_bytes(), "pproenca/tap").is_err());

    let duplicate_sha = format!(
        r#"{AGENT_TUI}
[[packages]]
token = "other"
version = "2.0"
bin = "other"

[packages.artifacts.arm64]
url = "https://example.com/other"
sha256 = "0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5"
"#
    );
    assert!(TapManifest::parse(duplicate_sha.as_bytes(), "pproenca/tap").is_err());

    let unknown_arch =
        single_artifact_manifest("aarch64", "https://example.com/tool", &"c".repeat(64));
    assert!(TapManifest::parse(unknown_arch.as_bytes(), "owner/tap").is_err());

    let no_artifacts = r#"schema = 1
tap = "owner/tap"

[[packages]]
token = "tool"
version = "1.0"
bin = "tool"
"#;
    assert!(TapManifest::parse(no_artifacts.as_bytes(), "owner/tap").is_err());
}

#[test]
fn manifest_rejects_invalid_reserved_names_and_unsafe_urls() {
    for token in ["bin", "brew", "install", "Tool", "../tool", "tool..x"] {
        let manifest =
            single_artifact_manifest("arm64", "https://example.com/tool", &"d".repeat(64))
                .replace("token = \"tool\"", &format!("token = \"{token}\""));
        assert!(
            TapManifest::parse(manifest.as_bytes(), "owner/tap").is_err(),
            "expected token {token} to be rejected"
        );
    }

    for bin in ["brew", "taproom", "Tool", "../tool"] {
        let manifest =
            single_artifact_manifest("arm64", "https://example.com/tool", &"e".repeat(64))
                .replace("bin = \"tool\"", &format!("bin = \"{bin}\""));
        assert!(
            TapManifest::parse(manifest.as_bytes(), "owner/tap").is_err(),
            "expected bin {bin} to be rejected"
        );
    }

    for url in [
        "http://example.com/tool",
        "file:///tmp/tool",
        "https://user:pass@example.com/tool",
        "https://127.0.0.1/tool",
        "https://example.com/tool#fragment",
    ] {
        let manifest = single_artifact_manifest("arm64", url, &"f".repeat(64));
        assert!(
            TapManifest::parse(manifest.as_bytes(), "owner/tap").is_err(),
            "expected URL {url} to be rejected"
        );
    }

    let bad_digest = single_artifact_manifest("arm64", "https://example.com/tool", "ABC");
    assert!(TapManifest::parse(bad_digest.as_bytes(), "owner/tap").is_err());
}

#[test]
fn records_and_receipts_validate_exact_identity() {
    let source = Path::new("/tmp/agent-tui.toml");
    let digest = "a".repeat(64);
    let record = TapRecord::new("pproenca/tap", source, &digest).unwrap();
    assert_eq!(record.schema, 1);
    assert!(record.validate("pproenca/tap").is_ok());
    assert!(record.validate("other/tap").is_err());

    let receipt = TapReceipt::new(
        "pproenca/tap",
        "agent-tui",
        "1.1.0",
        "agent-tui",
        "arm64",
        "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64",
        "0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5",
        1024,
        &digest,
        source,
    )
    .unwrap();
    assert_eq!(receipt.qualified, "pproenca/tap/agent-tui");
    assert_eq!(receipt.keg, "Taproom/pproenca/tap/agent-tui/1.1.0");
    assert_eq!(receipt.link, "bin/agent-tui");

    let encoded = serde_json::to_vec(&receipt).unwrap();
    let decoded: TapReceipt = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, receipt);

    let mut tampered = receipt.clone();
    tampered.keg = "Taproom/other/tap/agent-tui/1.1.0".into();
    assert!(tampered.validate().is_err());

    let mut tampered = receipt;
    tampered.artifact_bytes = 0;
    assert!(tampered.validate().is_err());
}

#[test]
fn host_architecture_is_macos_only() {
    let result = host_arch();
    if std::env::consts::OS == "macos" {
        assert!(matches!(result, Ok("arm64" | "x86_64")));
    } else {
        assert!(result.is_err());
    }
}
