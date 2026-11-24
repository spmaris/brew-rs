use super::*;
use std::cmp::Ordering::{Equal, Greater, Less};

fn compare_ok(candidate: &str, installed: &str) -> Ordering {
    compare(candidate, installed).expect("version should parse")
}

fn compare_err(candidate: &str, installed: &str) -> String {
    compare(candidate, installed)
        .expect_err("version should be rejected")
        .to_string()
}

#[test]
fn revision_is_compared_only_when_the_base_matches() {
    assert_eq!(compare_ok("1.2.3", "1.2.3"), Equal);
    assert_eq!(compare_ok("1.2.3_0", "1.2.3"), Equal);
    assert_eq!(compare_ok("1.2.3", "1.2.3_0"), Equal);
    assert_eq!(compare_ok("1.2.3_1", "1.2.3"), Greater);
    assert_eq!(compare_ok("1.2.3", "1.2.3_2"), Less);
    assert_eq!(compare_ok("1.2.3_10", "1.2.3_9"), Greater);
    assert_eq!(compare_ok("1.2.3_9", "1.2.3_10"), Less);
    assert_eq!(compare_ok("1.2.3_01", "1.2.3_1"), Equal);
    // A large revision cannot mask a lower base version.
    assert_eq!(compare_ok("1.2_99", "1.3"), Less);
    assert_eq!(compare_ok("1.2.1_0", "1.2"), Greater);
}

#[test]
fn numeric_components_are_normalized() {
    assert_eq!(compare_ok("1.0.0", "1"), Equal);
    assert_eq!(compare_ok("1", "1.0.0.0"), Equal);
    assert_eq!(compare_ok("1.02", "1.2"), Equal);
    assert_eq!(compare_ok("0.0", "0"), Equal);
    assert_eq!(compare_ok("1.10", "1.9"), Greater);
    assert_eq!(compare_ok("1.9", "1.10"), Less);
    assert_eq!(compare_ok("1.0.1", "1"), Greater);
    // Equal ordering does not imply equal text; the parent decides.
    assert_eq!(compare_ok("1.0", "1"), Equal);
}

#[test]
fn downgrades_and_upgrades_order_numerically() {
    assert_eq!(compare_ok("1.2.3", "1.2.4"), Less);
    assert_eq!(compare_ok("1.2.4", "1.2.3"), Greater);
    assert_eq!(compare_ok("2.0", "10.0"), Less);
    assert_eq!(compare_ok("10.0", "2.0"), Greater);
    assert_eq!(compare_ok("1.0.0_1", "1.0.1"), Less);
    assert_eq!(compare_ok("1.0.1", "1.0.0_1"), Greater);
}

#[test]
fn huge_components_compare_without_overflow() {
    let smaller = "9".repeat(30);
    let larger = format!("1{}", "0".repeat(30));
    assert_eq!(compare_ok(&smaller, &larger), Less);
    assert_eq!(compare_ok(&larger, &smaller), Greater);
    assert_eq!(compare_ok(&smaller, &smaller), Equal);

    // The 64-byte cap still admits a 64-digit component.
    let longest = "9".repeat(MAX_VERSION_LEN);
    let just_above = "1".to_string() + &"0".repeat(MAX_VERSION_LEN - 1);
    assert_eq!(compare_ok(&longest, &just_above), Greater);
    assert_eq!(compare_ok(&just_above, &longest), Less);
}

#[test]
fn prerelease_date_and_build_metadata_are_rejected() {
    for version in [
        "1.2.3-beta",
        "1.2.3-rc.1",
        "2024-01-01",
        "1.2.3+build",
        "1.2.3~rc",
    ] {
        let error = compare_err(version, "1.2.3");
        assert!(
            error.contains(version),
            "{version:?} error should name the version: {error}"
        );
        assert!(
            error.to_lowercase().contains("supported"),
            "{version:?} error should explain support: {error}"
        );
    }
}

#[test]
fn malformed_numeric_syntax_is_rejected() {
    for version in [
        "", ".", ".1", "1.", "1..2", "1.2.", "v1.2", "1.2a", "1.2_", "_1", "1.2_3_4", "1.2_-1",
        "1.2_+1", "1.2_3a", "1,2", "1 2", " 1.2", "1.2 ", "1.2\n", "1.٢",
    ] {
        assert!(
            compare(version, "1.2").is_err(),
            "{version:?} should be rejected"
        );
    }
    assert!(compare("1.2", "1.2.3-rc1").is_err());
}

#[test]
fn length_component_and_revision_caps_are_enforced() {
    let too_long = "1".repeat(MAX_VERSION_LEN + 1);
    let error = compare_err(&too_long, "1");
    assert!(error.contains(&MAX_VERSION_LEN.to_string()), "{error}");

    let too_many = vec!["1"; MAX_COMPONENTS + 1].join(".");
    let error = compare_err(&too_many, "1");
    assert!(error.contains(&MAX_COMPONENTS.to_string()), "{error}");

    let long_revision = format!("1.2_{}", "9".repeat(MAX_REVISION_DIGITS + 1));
    let error = compare_err(&long_revision, "1.2");
    assert!(error.contains(&MAX_REVISION_DIGITS.to_string()), "{error}");

    // Exactly at each cap is accepted.
    let at_length = "1".repeat(MAX_VERSION_LEN);
    assert_eq!(compare_ok(&at_length, &at_length), Equal);
    let at_count = vec!["1"; MAX_COMPONENTS].join(".");
    assert_eq!(compare_ok(&at_count, &at_count), Equal);
    let at_revision = format!("1.2_{}", "9".repeat(MAX_REVISION_DIGITS));
    assert_eq!(compare_ok(&at_revision, &at_revision), Equal);
}

#[test]
fn ordering_is_antisymmetric_for_supported_versions() {
    let versions = [
        "0", "0.0.1", "1", "1.0", "1.0.0_1", "1.0.1", "1.2", "1.10", "2", "10.0",
    ];
    for left in versions {
        for right in versions {
            let forward = compare_ok(left, right);
            let backward = compare_ok(right, left);
            assert_eq!(forward, backward.reverse(), "{left} vs {right}");
        }
    }
}

#[test]
fn errors_name_the_rejected_revision() {
    let error = compare_err("1.2.3_4x", "1.2.3");
    assert!(error.contains("1.2.3_4x"), "{error}");
    assert!(error.contains("revision"), "{error}");
}
