use super::*;
use crate::infra::store::Dependency;

fn receipt() -> Receipt {
    Receipt {
        source: Default::default(),
        name: "ripgrep".into(),
        version: "16.0.0".into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: vec![Dependency {
            name: "pcre2".into(),
            version: "10.49".into(),
            sha256: "b".repeat(64),
        }],
    }
}

#[test]
fn relocates_opt_references_to_the_exact_pinned_keg() {
    let receipt = receipt();
    let paths = Paths::new(
        Path::new("/home/example/Application Support/native"),
        &receipt,
    )
    .unwrap();
    let img = Path::new("bin/ripgrep");
    assert_eq!(
        paths
            .macho_path("@@HOMEBREW_PREFIX@@/opt/pcre2/lib/libpcre2-8.0.dylib", img)
            .unwrap()
            .as_deref(),
        Some("/home/example/Application Support/native/Cellar/pcre2/10.49/lib/libpcre2-8.0.dylib")
    );
    assert_eq!(
        paths
            .keg_path("@@HOMEBREW_CELLAR@@/ripgrep/16.0.0")
            .unwrap()
            .as_deref(),
        Some("/home/example/Application Support/native/Cellar/ripgrep/16.0.0")
    );
    assert_eq!(
        paths
            .keg_path("@@HOMEBREW_CELLAR@@/pcre2/10.49/lib")
            .unwrap()
            .as_deref(),
        Some("/home/example/Application Support/native/Cellar/pcre2/10.49/lib")
    );
}

#[test]
fn preserves_only_normalized_system_paths() {
    let receipt = receipt();
    let paths = Paths::new(Path::new("/tmp/prefix"), &receipt).unwrap();
    let img = Path::new("bin/ripgrep");
    for system in [
        "/usr/lib/libbz2.1.0.dylib",
        "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
    ] {
        assert_eq!(paths.macho_path(system, img).unwrap(), None);
    }
    for unsupported in [
        "/usr/lib/../../opt/homebrew/lib/evil.dylib",
        "/opt/homebrew/lib/libpcre2.dylib",
        "@rpath/library.dylib",
        "@@HOMEBREW_PREFIX@@/lib/library.dylib",
        "relative.dylib",
    ] {
        assert!(paths.macho_path(unsupported, img).is_err(), "{unsupported}");
    }
}

#[test]
fn refuses_a_different_or_unpinned_keg_and_traversal() {
    let receipt = receipt();
    let paths = Paths::new(Path::new("/tmp/prefix"), &receipt).unwrap();
    let img = Path::new("bin/ripgrep");
    for malformed in [
        "@@HOMEBREW_CELLAR@@/pcre2/10.48/lib/libpcre2.dylib",
        "@@HOMEBREW_PREFIX@@/opt/openssl@3/lib/libssl.dylib",
        "@@HOMEBREW_PREFIX@@/opt/pcre2/../lib.dylib",
        "@@HOMEBREW_CELLAR@@/pcre2/10.49//lib",
        "@@HOMEBREW_CELLAR@@/pcre2/10.49/",
    ] {
        assert!(paths.macho_path(malformed, img).is_err(), "{malformed}");
    }
}

#[test]
fn rejects_prefixes_unsafe_in_supported_text_metadata() {
    let receipt = receipt();
    for bad in [
        "relative",
        "/",
        "/tmp/../prefix",
        "/tmp//prefix",
        "/tmp/$HOME",
        "/tmp/'quoted'",
        "/tmp/line\nbreak",
    ] {
        assert!(Paths::new(Path::new(bad), &receipt).is_err(), "{bad}");
    }
}

#[test]
fn resolves_rpaths_loader_paths_and_rpath_members() {
    let sqlite_receipt = Receipt {
        source: Default::default(),
        name: "sqlite".into(),
        version: "3.54.1".into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: true,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    };
    let sqlite_paths = Paths::new(Path::new("/tmp/Space Prefix"), &sqlite_receipt).unwrap();
    assert_eq!(
        sqlite_paths
            .rpath_path(
                "@@HOMEBREW_CELLAR@@/sqlite/3.54.1/lib",
                Path::new("bin/sqlite3")
            )
            .unwrap(),
        RpathTarget::KegDir("/tmp/Space Prefix/Cellar/sqlite/3.54.1/lib".into())
    );

    let zstd_receipt = Receipt {
        source: Default::default(),
        name: "zstd".into(),
        version: "1.5.7_1".into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    };
    let zstd_paths = Paths::new(Path::new("/tmp/Space Prefix"), &zstd_receipt).unwrap();
    assert_eq!(
        zstd_paths
            .rpath_path("@loader_path/../lib", Path::new("bin/zstd"))
            .unwrap(),
        RpathTarget::KegDir("/tmp/Space Prefix/Cellar/zstd/1.5.7_1/lib".into())
    );
    assert_eq!(
        zstd_paths
            .rpath_path("@loader_path/../lib", Path::new("lib/libzstd.1.5.7.dylib"))
            .unwrap(),
        RpathTarget::KegDir("/tmp/Space Prefix/Cellar/zstd/1.5.7_1/lib".into())
    );

    let luajit_receipt = Receipt {
        source: Default::default(),
        name: "luajit".into(),
        version: "2.1.1795119671".into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    };
    let luajit_paths = Paths::new(Path::new("/tmp/Space Prefix"), &luajit_receipt).unwrap();
    assert_eq!(
        luajit_paths
            .rpath_path("@loader_path/../../../../lib", Path::new("bin/luajit-2.1"))
            .unwrap(),
        RpathTarget::PrefixLib("/tmp/Space Prefix/lib".into())
    );
    assert_eq!(
        luajit_paths
            .rpath_path(
                "@loader_path/../../../../lib",
                Path::new("lib/libluajit-5.1.dylib")
            )
            .unwrap(),
        RpathTarget::PrefixLib("/tmp/Space Prefix/lib".into())
    );
    assert!(
        luajit_paths
            .rpath_path(
                "@loader_path/../../../../../etc",
                Path::new("bin/luajit-2.1")
            )
            .is_err()
    );
    assert!(
        luajit_paths
            .rpath_path(
                "@loader_path/../../../../share",
                Path::new("bin/luajit-2.1")
            )
            .is_err()
    );

    let icu_receipt = Receipt {
        source: Default::default(),
        name: "icu4c@79".into(),
        version: "79.1".into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: true,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    };
    let icu_paths = Paths::new(Path::new("/tmp/Space Prefix"), &icu_receipt).unwrap();
    assert_eq!(
        icu_paths
            .macho_path(
                "@loader_path/libicuuc.79.dylib",
                Path::new("lib/libicui18n.79.1.dylib")
            )
            .unwrap()
            .as_deref(),
        Some("/tmp/Space Prefix/Cellar/icu4c@79/79.1/lib/libicuuc.79.dylib")
    );
    assert!(
        icu_paths
            .macho_path(
                "@loader_path/../../other.dylib",
                Path::new("lib/libicui18n.79.1.dylib")
            )
            .is_err()
    );

    assert_eq!(
        Paths::rpath_member("@rpath/libzstd.1.dylib").unwrap(),
        Some("libzstd.1.dylib")
    );
    assert_eq!(
        Paths::rpath_member("/usr/lib/libSystem.B.dylib").unwrap(),
        None
    );
    assert!(Paths::rpath_member("@rpath/").is_err());
    assert!(Paths::rpath_member("@rpath/../evil.dylib").is_err());
}
