use super::*;
use crate::infra::store::{Receipt, Store};

#[test]
fn rejected_relocation_never_publishes_the_keg_or_receipt() {
    let sandbox = tempfile::tempdir().unwrap();
    let prefix = sandbox.path().join("Application Support").join("native");
    let store = Store::open(&prefix, true).unwrap();
    let stage = store.stage().unwrap();
    let staged_path = stage.path().to_path_buf();
    fs::write(
        stage.path().join("opaque.dat"),
        b"\0@@HOMEBREW_PREFIX@@/lib",
    )
    .unwrap();
    let package = Prepared {
        stage,
        receipt: Receipt {
            source: Default::default(),
            name: "ripgrep".into(),
            version: "16.0.0".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        },
    };
    let result = staged(
        &prefix,
        [Item {
            package: &package,
            needs_relocation: true,
        }],
    );
    assert!(format!("{:#}", anyhow::Error::new(result.unwrap_err())).contains("opaque.dat"));
    assert!(store.installed().unwrap().is_empty());
    assert!(!prefix.join("Cellar/ripgrep").exists());
    drop(package);
    assert!(!staged_path.exists());
}

#[test]
fn validates_staged_and_installed_targets_without_following_escapes() {
    let prefix = tempfile::tempdir().unwrap();
    let staged = tempfile::tempdir().unwrap();
    let dst = prefix.path().join("Cellar/pcre2/10.49/lib/libpcre2.dylib");
    let keg_root_dst = prefix.path().join("Cellar/pcre2/10.49");
    let lib_dir_dst = prefix.path().join("Cellar/pcre2/10.49/lib");
    let batch = HashMap::from([(("pcre2", "10.49"), staged.path())]);
    fs::create_dir(staged.path().join("lib")).unwrap();
    assert!(validate_target(prefix.path(), &dst, &batch, TargetKind::File).is_err());
    validate_target(prefix.path(), &lib_dir_dst, &batch, TargetKind::Dir).unwrap();
    validate_target(prefix.path(), &keg_root_dst, &batch, TargetKind::Dir).unwrap();
    #[cfg(unix)]
    {
        let outside = prefix.path().join("foreign.dylib");
        fs::write(&outside, b"unrelated").unwrap();
        std::os::unix::fs::symlink(&outside, staged.path().join("lib/escaped.dylib")).unwrap();
        assert!(
            validate_target(
                prefix.path(),
                &dst.with_file_name("escaped.dylib"),
                &batch,
                TargetKind::File
            )
            .is_err()
        );
        let outside_dir = prefix.path().join("foreign-dir");
        fs::create_dir(&outside_dir).unwrap();
        std::os::unix::fs::symlink(&outside_dir, staged.path().join("escaped-dir")).unwrap();
        assert!(
            validate_target(
                prefix.path(),
                &keg_root_dst.join("escaped-dir"),
                &batch,
                TargetKind::Dir
            )
            .is_err()
        );
    }
    fs::write(staged.path().join("lib/libpcre2.dylib"), b"fixture").unwrap();
    validate_target(prefix.path(), &dst, &batch, TargetKind::File).unwrap();
    assert!(validate_target(prefix.path(), &dst, &batch, TargetKind::Dir).is_err());
    let empty = HashMap::new();
    assert!(validate_target(prefix.path(), &dst, &empty, TargetKind::File).is_err());
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::write(&dst, b"fixture").unwrap();
    validate_target(prefix.path(), &dst, &empty, TargetKind::File).unwrap();
    validate_target(prefix.path(), &lib_dir_dst, &empty, TargetKind::Dir).unwrap();
    assert!(
        validate_target(
            prefix.path(),
            dst.parent().unwrap(),
            &empty,
            TargetKind::File
        )
        .is_err()
    );
}
