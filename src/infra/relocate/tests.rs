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

#[test]
fn io_errors_name_the_action_and_path() {
    let error = RelocateError::io("read directory", Path::new("/stage/lib"))(
        std::io::Error::other("denied"),
    );
    assert_eq!(
        crate::infra::error::error_chain(&error),
        "failed to read directory '/stage/lib': denied"
    );
}

fn small_limits() -> Limits {
    Limits {
        text: 64,
        macho: 256,
        chunk: 8,
    }
}

fn big_file(dir: &Path, name: &str, len: usize, patch: impl FnOnce(&mut Vec<u8>)) -> PathBuf {
    let mut bytes = vec![b'a'; len];
    patch(&mut bytes);
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn too_large(result: Result<Loaded, RelocateError>) -> Option<u64> {
    match result {
        Err(RelocateError::FileTooLarge { limit }) => Some(limit),
        _ => None,
    }
}

#[test]
fn oversized_text_without_markers_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = big_file(dir.path(), "big.dat", 1000, |_| {});
    assert!(matches!(load(&path, small_limits()), Ok(Loaded::Skip)));
    let small = big_file(dir.path(), "small.dat", 64, |_| {});
    assert!(matches!(load(&small, small_limits()), Ok(Loaded::Text(b)) if b.len() == 64));
}

#[test]
fn oversized_text_with_marker_anywhere_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let marker = b"@@HOMEBREW_PREFIX@@";
    // Offsets: start, straddling every chunk boundary phase, and the very end.
    let mut offsets = vec![0, 1000 - marker.len()];
    offsets.extend(8 - 5..8 + 1);
    offsets.extend(500 - 7..500 + 1);
    for (i, offset) in offsets.into_iter().enumerate() {
        let path = big_file(dir.path(), &format!("m{i}"), 1000, |b| {
            b[offset..offset + marker.len()].copy_from_slice(marker);
        });
        assert_eq!(
            too_large(load(&path, small_limits())),
            Some(64),
            "offset {offset}"
        );
    }
}

#[test]
fn stream_scan_matches_in_memory_search() {
    for chunk in 1..30 {
        for offset in 0..40 {
            let mut bytes = [b'x'; 60];
            bytes[offset..offset + 11].copy_from_slice(b"@@HOMEBREW_");
            assert!(text::stream_has_needle(&bytes[..], chunk).unwrap());
            bytes[offset + 10] = b'!';
            assert!(!text::stream_has_needle(&bytes[..], chunk).unwrap());
        }
    }
}

#[test]
fn macho_is_detected_from_the_prefix_alone() {
    let dir = tempfile::tempdir().unwrap();
    let thin = |b: &mut Vec<u8>| b[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    // Within the Mach-O cap but above the text cap: read in full, not skipped.
    let path = big_file(dir.path(), "tool", 200, thin);
    assert!(matches!(load(&path, small_limits()), Ok(Loaded::Macho(b)) if b.len() == 200));
    // Above the Mach-O cap.
    let path = big_file(dir.path(), "huge", 300, thin);
    assert_eq!(too_large(load(&path, small_limits())), Some(256));
    // A large Mach-O is rejected on size even when it has no marker.
    let limits = Limits {
        macho: 1 << 20,
        ..small_limits()
    };
    let path = big_file(dir.path(), "mid", 5000, thin);
    assert!(matches!(load(&path, limits), Ok(Loaded::Macho(_))));
}

#[test]
fn large_fat_image_is_recognized_with_only_a_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut fat = vec![0u8; 20_000];
    fat[..4].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
    fat[4..8].copy_from_slice(&1u32.to_be_bytes());
    fat[8..12].copy_from_slice(&0x0100_000cu32.to_be_bytes());
    fat[16..20].copy_from_slice(&16_384u32.to_be_bytes());
    fat[20..24].copy_from_slice(&3_000u32.to_be_bytes());
    fat[24..28].copy_from_slice(&14u32.to_be_bytes());
    let path = dir.path().join("fat");
    fs::write(&path, &fat).unwrap();
    let limits = Limits {
        macho: 10_000,
        ..small_limits()
    };
    assert_eq!(too_large(load(&path, limits)), Some(10_000));
    assert!(macho::is_macho(&fat));
    // A slice running past the file end is not FAT, so it takes the text path.
    fat[20..24].copy_from_slice(&30_000u32.to_be_bytes());
    fs::write(&path, &fat).unwrap();
    assert!(matches!(load(&path, limits), Ok(Loaded::Skip)));
}
