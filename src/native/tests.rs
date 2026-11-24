use super::*;

#[test]
fn macos_platform_keys_match_homebrew() {
    assert_eq!(macos_tag("aarch64", "15.7.1").unwrap(), "arm64_sequoia");
    assert_eq!(macos_tag("x86_64", "15.7").unwrap(), "sequoia");
    assert_eq!(macos_tag("aarch64", "26.1").unwrap(), "arm64_tahoe");
    assert!(macos_tag("aarch64", "27.0").is_err());
}
