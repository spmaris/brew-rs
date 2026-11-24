use super::error::error_chain;
use super::paths::{cellar_dir, opt_dir, receipts_dir, sbin_dir};
use super::receipt::{
    encode_receipt, is_safe_bin_name, is_safe_package_name, read_receipt, replace_receipt,
    validate_receipt,
};
use super::*;
use crate::domain::plan::PackageSource;
use crate::domain::validate::{is_lower_hex_digest, is_safe_version};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::{TempDir, tempdir};

fn digest() -> String {
    "a".repeat(64)
}

fn receipt(name: &str, version: &str, requested: bool) -> Receipt {
    Receipt {
        source: Default::default(),
        name: name.into(),
        version: version.into(),
        version_scheme: 0,
        sha256: digest(),
        requested,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    }
}

fn dependency(receipt: &Receipt) -> Dependency {
    Dependency {
        name: receipt.name.clone(),
        version: receipt.version.clone(),
        sha256: receipt.sha256.clone(),
    }
}

fn new_store() -> (TempDir, Store) {
    let temp = tempdir().expect("temporary directory");
    let prefix = temp.path().join("prefix");
    let store = Store::open(&prefix, true).expect("create store");
    (temp, store)
}

fn prepare(store: &Store, name: &str, version: &str, bins: &[(&str, bool)]) -> Prepared {
    let stage = store.stage().expect("stage directory");
    let bin = stage.path().join(BIN);
    fs::create_dir_all(&bin).expect("create staged bin");
    for (member, executable) in bins {
        let path = bin.join(member);
        fs::write(&path, b"#!/bin/sh\n").expect("write staged member");
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if *executable { 0o755 } else { 0o644 }),
        )
        .expect("set staged mode");
    }
    let lib = stage.path().join("lib");
    fs::create_dir_all(&lib).expect("create staged lib");
    fs::write(lib.join(format!("{name}.dylib")), b"payload").expect("write staged payload");
    Prepared {
        receipt: receipt(name, version, true),
        stage,
    }
}

fn install(store: &Store, name: &str, version: &str, bins: &[(&str, bool)]) -> Receipt {
    let prepared = prepare(store, name, version, bins);
    let receipt = prepared.receipt.clone();
    store.commit(vec![prepared]).expect("install package");
    receipt
}

/// Write one staged file at a keg-relative path and set its mode.
fn stage_file(prepared: &Prepared, relative: &str, bytes: &[u8], mode: u32) {
    let path = prepared.stage.path().join(relative);
    fs::create_dir_all(path.parent().expect("staged parent")).expect("create staged directory");
    fs::write(&path, bytes).expect("write staged file");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set staged mode");
}

/// Create one keg-contained relative symlink in a stage.
#[cfg(unix)]
fn stage_link(prepared: &Prepared, relative: &str, target: &str) {
    let path = prepared.stage.path().join(relative);
    fs::create_dir_all(path.parent().expect("staged parent")).expect("create staged directory");
    std::os::unix::fs::symlink(target, &path).expect("create staged symlink");
}

/// The exact target a commit stores for a managed `relative` path.
fn keg_target(name: &str, version: &str, relative: &str) -> PathBuf {
    let mut target = PathBuf::new();
    for _ in 1..Path::new(relative).components().count() {
        target.push("..");
    }
    target.join(CELLAR).join(name).join(version).join(relative)
}

/// Install a package as an unrequested dependency, as a commit of a
/// dependency-first plan would.
fn install_unrequested(store: &Store, name: &str, version: &str, bins: &[(&str, bool)]) -> Receipt {
    let mut prepared = prepare(store, name, version, bins);
    prepared.receipt.requested = false;
    let receipt = prepared.receipt.clone();
    store.commit(vec![prepared]).expect("install dependency");
    receipt
}

fn prepare_with(
    store: &Store,
    name: &str,
    version: &str,
    bins: &[(&str, bool)],
    dependencies: &[Dependency],
) -> Prepared {
    let mut prepared = prepare(store, name, version, bins);
    prepared.receipt.dependencies = dependencies.to_vec();
    prepared
}

fn install_with_dependency(
    store: &Store,
    name: &str,
    version: &str,
    dependency_on: &Receipt,
    bins: &[(&str, bool)],
) -> Receipt {
    let prepared = prepare_with(store, name, version, bins, &[dependency(dependency_on)]);
    let receipt = prepared.receipt.clone();
    store
        .commit(vec![prepared])
        .expect("install dependent package");
    receipt
}

fn read_only(store: &Store) -> Store {
    Store::open(store.prefix(), false).expect("read-only reopen")
}

/// Every path below `root`, with link targets and file bytes, for exact
/// before/after comparisons.
fn tree(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(root: &Path, directory: &Path, out: &mut Vec<(PathBuf, String)>) {
        for entry in fs::read_dir(directory).expect("read directory") {
            let path = entry.expect("entry").path();
            let metadata = fs::symlink_metadata(&path).expect("metadata");
            let relative = path.strip_prefix(root).expect("relative").to_path_buf();
            if metadata.file_type().is_symlink() {
                out.push((
                    relative,
                    format!("link:{}", fs::read_link(&path).expect("link").display()),
                ));
            } else if metadata.is_dir() {
                out.push((relative, "directory".into()));
                walk(root, &path, out);
            } else {
                out.push((
                    relative,
                    format!(
                        "file:{}",
                        String::from_utf8_lossy(&fs::read(&path).expect("file")).escape_debug()
                    ),
                ));
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort();
    entries
}

#[test]
fn read_only_binding_of_an_absent_prefix_is_empty_and_writes_nothing() {
    let temp = tempdir().expect("temporary directory");
    let prefix = temp.path().join("absent");
    let store = Store::open(&prefix, false).expect("bind absent prefix");

    assert_eq!(store.prefix(), prefix);
    assert!(store.installed().expect("installed").is_empty());
    assert!(!prefix.exists());
    assert!(store.stage().is_err());
    assert!(store.commit(Vec::new()).is_err());
    assert!(!prefix.exists());
}

#[test]
fn creates_the_marker_and_every_owned_directory() {
    let temp = tempdir().expect("temporary directory");
    let prefix = temp.path().join("prefix");
    let store = Store::open(&prefix, true).expect("create store");

    assert_eq!(store.prefix(), prefix);
    assert_eq!(
        fs::read(prefix.join(MARKER_FILE)).expect("marker"),
        MARKER_CONTENT.as_bytes()
    );
    for directory in [
        cellar_dir(&prefix),
        bin_dir(&prefix),
        sbin_dir(&prefix),
        opt_dir(&prefix),
        meta_dir(&prefix),
        state_dir(&prefix),
        receipts_dir(&prefix),
        stages_dir(&prefix),
    ] {
        assert!(
            directory.is_dir(),
            "{} should be a directory",
            directory.display()
        );
    }
    assert!(store.installed().expect("installed").is_empty());
    drop(store);

    let reopened = Store::open(&prefix, false).expect("read-only reopen");
    assert!(reopened.installed().expect("installed").is_empty());
}

#[test]
fn initializes_an_existing_empty_directory() {
    let temp = tempdir().expect("temporary directory");
    let prefix = temp.path().join("empty");
    fs::create_dir(&prefix).expect("create empty prefix");

    Store::open(&prefix, true).expect("adopt empty directory");
    assert!(prefix.join(MARKER_FILE).is_file());
}

#[cfg(unix)]
#[test]
fn refuses_a_symlinked_prefix() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().expect("temporary directory");
    let real = temp.path().join("real");
    fs::create_dir(&real).expect("create real prefix");
    let link = temp.path().join("link");
    symlink(&real, &link).expect("symlink prefix");

    assert!(Store::open(&link, true).is_err());
    assert!(Store::open(&link, false).is_err());
    assert!(!real.join(MARKER_FILE).exists());
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_managed_directories() {
    use std::os::unix::fs::symlink;

    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();
    drop(store);
    let outside = tempdir().expect("outside directory");
    fs::remove_dir_all(prefix.join(META)).expect("remove state directory");
    symlink(outside.path(), prefix.join(META)).expect("symlink state directory");

    assert!(Store::open(&prefix, true).is_err());
    let read_only = Store::open(&prefix, false).expect("bind marked prefix");
    assert!(read_only.installed().is_err());
}

#[cfg(unix)]
#[test]
fn resolves_dangling_symlinks_when_checking_protection() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().expect("temporary directory");
    let target = temp.path().join("outside").join("missing");
    let link = temp.path().join("link");
    symlink(&target, &link).expect("dangling symlink");

    let resolved = canonical_candidate(&link.join("sub")).expect("resolve candidate");
    assert_eq!(
        resolved,
        temp.path()
            .canonicalize()
            .expect("canonical temp")
            .join("outside")
            .join("missing")
            .join("sub")
    );
}

#[test]
fn refuses_a_nonempty_unowned_prefix_without_touching_it() {
    let temp = tempdir().expect("temporary directory");
    let prefix = temp.path().join("prefix");
    fs::create_dir(&prefix).expect("create prefix");
    fs::write(prefix.join("keep"), b"x").expect("write foreign file");

    let error = Store::open(&prefix, true).expect_err("unowned prefix must fail");
    assert!(error_chain(&error).contains("not empty"), "{error:#}");
    assert!(prefix.join("keep").exists());
    assert!(!prefix.join(MARKER_FILE).exists());
}

#[test]
fn refuses_known_homebrew_prefixes() {
    assert!(Store::open(Path::new("/opt/homebrew"), true).is_err());
    assert!(Store::open(Path::new("/usr/local"), true).is_err());
    assert!(Store::open(Path::new("/home/linuxbrew/.linuxbrew"), true).is_err());

    let unique = format!("brewrs-store-test-{}", std::process::id());
    let candidate = Path::new("/usr/local").join(unique);
    let error = Store::open(&candidate, true).expect_err("Homebrew descendant must fail");
    assert!(error_chain(&error).contains("Homebrew"), "{error:#}");
    assert!(!candidate.exists());
}

#[test]
fn refuses_a_foreign_marker() {
    let (_temp, store) = new_store();
    fs::write(store.prefix().join(MARKER_FILE), b"foreign\n").expect("replace marker");
    assert!(Store::open(store.prefix(), false).is_err());
    assert!(Store::open(store.prefix(), true).is_err());
}

#[test]
fn commit_installs_files_links_and_a_receipt() {
    let (_temp, store) = new_store();
    let prepared = prepare(
        &store,
        "hello",
        "1.0",
        &[
            ("hello", true),
            ("readme", false),
            (".hidden", true),
            ("bad name", true),
        ],
    );
    let stage_root = prepared.stage.path().to_path_buf();
    let expected = prepared.receipt.clone();

    store.commit(vec![prepared]).expect("commit package");

    assert!(
        !stage_root.exists(),
        "the stage must be moved into the Cellar"
    );
    let keg = store.prefix().join(CELLAR).join("hello").join("1.0");
    assert_eq!(
        fs::read(keg.join("bin").join("hello")).expect("keg binary"),
        b"#!/bin/sh\n"
    );
    assert_eq!(
        fs::read(keg.join("lib").join("hello.dylib")).expect("keg payload"),
        b"payload"
    );

    let link = store.prefix().join(BIN).join("hello");
    assert!(
        fs::symlink_metadata(&link)
            .expect("link metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(&link).expect("link target"),
        Path::new("../Cellar/hello/1.0/bin/hello")
    );
    for skipped in ["readme", ".hidden", "bad name"] {
        assert!(
            fs::symlink_metadata(store.prefix().join(BIN).join(skipped)).is_err(),
            "{skipped} must not be linked"
        );
    }

    assert_eq!(store.installed().expect("installed"), vec![expected]);
    assert!(
        store
            .prefix()
            .join(META)
            .join(STATE)
            .join(RECEIPTS)
            .join("hello.json")
            .is_file()
    );

    let prefix = store.prefix().to_path_buf();
    drop(store);
    let reopened = Store::open(&prefix, false).expect("read-only reopen");
    assert_eq!(reopened.installed().expect("installed").len(), 1);
    assert!(reopened.stage().is_err());
}

#[test]
fn installed_reports_receipts_sorted_by_name() {
    let (_temp, store) = new_store();
    let zeta = prepare(&store, "zeta", "2.0", &[("zeta", true)]);
    let mut alpha = prepare(&store, "alpha", "1.0", &[("alpha", true)]);
    alpha.receipt.requested = false;

    store.commit(vec![zeta, alpha]).expect("commit packages");
    let names: Vec<_> = store
        .installed()
        .expect("installed")
        .into_iter()
        .map(|receipt| (receipt.name, receipt.requested))
        .collect();
    assert_eq!(
        names,
        vec![("alpha".to_string(), false), ("zeta".to_string(), true)]
    );
}

#[test]
fn refuses_to_overwrite_an_existing_bin_path() {
    let (_temp, store) = new_store();
    let occupied = store.prefix().join(BIN).join("tool");
    fs::write(&occupied, b"keep").expect("occupy bin name");

    let error = store
        .commit(vec![prepare(&store, "alpha", "1.0", &[("tool", true)])])
        .expect_err("must not overwrite");
    assert!(error_chain(&error).contains("already exists"), "{error:#}");
    assert_eq!(fs::read(&occupied).expect("foreign file"), b"keep");
    assert!(store.installed().expect("installed").is_empty());
    assert!(!store.prefix().join(CELLAR).join("alpha").exists());
}

#[test]
fn refuses_duplicate_bin_names_across_one_commit() {
    let (_temp, store) = new_store();
    let error = store
        .commit(vec![
            prepare(&store, "alpha", "1.0", &[("shared", true)]),
            prepare(&store, "beta", "1.0", &[("shared", true)]),
        ])
        .expect_err("cross-package collision must fail");
    assert!(
        error_chain(&error).contains("more than one package"),
        "{error:#}"
    );
    assert!(store.installed().expect("installed").is_empty());
    assert!(
        fs::read_dir(store.prefix().join(BIN))
            .expect("read bin")
            .next()
            .is_none()
    );
}

#[test]
fn refuses_duplicate_package_names_in_one_commit() {
    let (_temp, store) = new_store();
    let error = store
        .commit(vec![
            prepare(&store, "hello", "1.0", &[("hello", true)]),
            prepare(&store, "hello", "1.0", &[("other", true)]),
        ])
        .expect_err("duplicate package name must fail");
    assert!(error_chain(&error).contains("more than once"), "{error:#}");
    assert!(store.installed().expect("installed").is_empty());
}

#[test]
fn refuses_to_replace_an_installed_version() {
    let (_temp, store) = new_store();
    store
        .commit(vec![prepare(&store, "hello", "1.0", &[("hello", true)])])
        .expect("first install");

    let error = store
        .commit(vec![prepare(&store, "hello", "2.0", &[("hello", true)])])
        .expect_err("different version must fail");
    assert!(
        error_chain(&error).contains("already installed"),
        "{error:#}"
    );

    let installed = store.installed().expect("installed");
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].version, "1.0");
    assert!(
        store
            .prefix()
            .join(CELLAR)
            .join("hello")
            .join("1.0")
            .is_dir()
    );
    assert!(
        !store
            .prefix()
            .join(CELLAR)
            .join("hello")
            .join("2.0")
            .exists()
    );
}

#[test]
fn refuses_the_same_version_and_leaves_idempotence_to_the_caller() {
    let (_temp, store) = new_store();
    store
        .commit(vec![prepare(&store, "hello", "1.0", &[("hello", true)])])
        .expect("first install");

    let error = store
        .commit(vec![prepare(&store, "hello", "1.0", &[("hello", true)])])
        .expect_err("same version must fail defensively");
    assert!(
        error_chain(&error).contains("already installed"),
        "{error:#}"
    );
    assert_eq!(store.installed().expect("installed").len(), 1);
}

#[test]
fn validates_receipts_before_writing_anything() {
    let (_temp, store) = new_store();
    let cases = [
        ("../evil".to_string(), "1.0".to_string(), digest()),
        ("hello".to_string(), "../1".to_string(), digest()),
        (
            "hello".to_string(),
            "1.0".to_string(),
            digest().to_uppercase(),
        ),
        ("hello".to_string(), "1.0".to_string(), "a".repeat(63)),
    ];
    for (name, version, sha256) in cases {
        let mut prepared = prepare(&store, "placeholder", "1.0", &[]);
        prepared.receipt.name = name;
        prepared.receipt.version = version;
        prepared.receipt.sha256 = sha256;
        assert!(store.commit(vec![prepared]).is_err());
    }
    assert!(store.installed().expect("installed").is_empty());
    assert!(
        fs::read_dir(store.prefix().join(CELLAR))
            .expect("read cellar")
            .next()
            .is_none()
    );
}

#[test]
fn refuses_a_stage_owned_by_another_directory() {
    let (_temp, store) = new_store();
    let foreign = tempdir().expect("foreign temporary directory");
    fs::create_dir_all(foreign.path().join(BIN)).expect("foreign bin");

    let error = store
        .commit(vec![Prepared {
            receipt: receipt("hello", "1.0", true),
            stage: foreign,
        }])
        .expect_err("foreign stage must fail");
    assert!(error_chain(&error).contains("not owned"), "{error:#}");
    assert!(store.installed().expect("installed").is_empty());
}

#[test]
fn lock_blocks_writers_and_explains_stale_locks() {
    let (_temp, store) = new_store();
    let lock = store.prefix().join(META).join(STATE).join(LOCK_FILE);
    fs::write(&lock, b"pid=1 nonce=stale\n").expect("write stale lock");

    let error = store
        .commit(vec![prepare(&store, "hello", "1.0", &[])])
        .expect_err("held lock must fail");
    let message = error_chain(&error);
    assert!(message.contains("stale"), "{message}");
    assert!(message.contains("remove"), "{message}");
    assert!(lock.exists());

    fs::remove_file(&lock).expect("clear stale lock");
    store
        .commit(vec![prepare(&store, "hello", "1.0", &[])])
        .expect("commit after clearing lock");
    assert!(!lock.exists(), "a successful commit drops its lock");
}

#[test]
fn rolls_back_every_owned_path_when_a_later_package_fails() {
    let (_temp, store) = new_store();
    let first = prepare(&store, "alpha", "1.0", &[("alpha", true)]);
    let second = prepare(&store, "beta", "1.0", &[("beta", true)]);

    let error = store
        .commit_inner(vec![first, second], Some(1))
        .expect_err("injected failure");
    assert!(error_chain(&error).contains("injected"), "{error:#}");

    assert!(store.installed().expect("installed").is_empty());
    assert!(!store.prefix().join(CELLAR).join("alpha").exists());
    assert!(!store.prefix().join(CELLAR).join("beta").exists());
    assert!(
        fs::read_dir(store.prefix().join(BIN))
            .expect("read bin")
            .next()
            .is_none()
    );
    assert!(
        !store
            .prefix()
            .join(META)
            .join(STATE)
            .join(LOCK_FILE)
            .exists()
    );
}

#[test]
fn removal_removes_only_the_selected_owned_paths() {
    use std::os::unix::fs::symlink;

    let (_temp, store) = new_store();
    let alpha = install(
        &store,
        "alpha",
        "1.0",
        &[("alpha", true), ("alpha-tool", true)],
    );
    let beta = install(&store, "beta", "1.0", &[("beta", true)]);
    let prefix = store.prefix().to_path_buf();

    fs::write(prefix.join("keep.txt"), b"keep").expect("foreign file");
    fs::write(prefix.join(BIN).join("note"), b"keep").expect("foreign bin file");
    let foreign = prefix.join(BIN).join("alpha-tool");
    fs::remove_file(&foreign).expect("drop owned link");
    symlink("/usr/bin/true", &foreign).expect("foreign link");

    let before = tree(&prefix);
    read_only(&store)
        .remove(std::slice::from_ref(&alpha))
        .expect("remove alpha");

    let mut expected = before;
    expected.retain(|(path, _)| {
        path.as_path() != Path::new(".brewrs/state/receipts/alpha.json")
            && !path.starts_with("Cellar/alpha")
            && path.as_path() != Path::new("bin/alpha")
            && path.as_path() != Path::new("opt/alpha")
    });
    assert_eq!(tree(&prefix), expected);
    assert_eq!(
        read_only(&store).installed().expect("installed"),
        vec![beta]
    );
    assert_eq!(
        fs::read_link(&foreign).expect("foreign link"),
        Path::new("/usr/bin/true")
    );
    assert_eq!(fs::read(prefix.join("keep.txt")).expect("keep"), b"keep");
    assert_eq!(
        fs::read(prefix.join(MARKER_FILE)).expect("marker"),
        MARKER_CONTENT.as_bytes()
    );
    assert!(
        fs::read_dir(prefix.join(META).join(STAGES))
            .expect("stages")
            .next()
            .is_none(),
        "a successful removal leaves no quarantine"
    );
    assert!(!prefix.join(META).join(STATE).join(LOCK_FILE).exists());
}

#[test]
fn removal_preflights_every_target_before_moving_anything() {
    let (_temp, store) = new_store();
    let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
    let beta = install(&store, "beta", "1.0", &[("beta", true)]);
    let prefix = store.prefix().to_path_buf();
    fs::remove_dir_all(prefix.join(CELLAR).join("beta").join("1.0")).expect("damage beta");
    let before = tree(&prefix);

    let error = read_only(&store)
        .remove(&[alpha, beta])
        .expect_err("a missing keg must fail preflight");
    assert!(error_chain(&error).contains("beta"), "{error:#}");
    assert_eq!(tree(&prefix), before);
    assert_eq!(read_only(&store).installed().expect("installed").len(), 2);
}

#[test]
fn removal_rejects_a_stale_plan() {
    let (_temp, store) = new_store();
    let mut stale = install(&store, "hello", "1.0", &[("hello", true)]);
    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    stale.version = "2.0".into();
    let error = read_only(&store)
        .remove(&[stale])
        .expect_err("stale receipt must fail");
    assert!(error_chain(&error).contains("re-read"), "{error:#}");
    assert_eq!(tree(&prefix), before);
}

#[test]
fn removal_rejects_duplicate_targets() {
    let (_temp, store) = new_store();
    let hello = install(&store, "hello", "1.0", &[("hello", true)]);
    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    let error = read_only(&store)
        .remove(&[hello.clone(), hello])
        .expect_err("duplicate target must fail");
    assert!(error_chain(&error).contains("more than once"), "{error:#}");
    assert_eq!(tree(&prefix), before);
}

#[test]
fn removal_handles_multiple_targets_in_one_call() {
    let (_temp, store) = new_store();
    let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
    let beta = install(&store, "beta", "1.0", &[("beta", true)]);
    let prefix = store.prefix().to_path_buf();

    read_only(&store)
        .remove(&[alpha, beta])
        .expect("remove both packages");

    assert!(read_only(&store).installed().expect("installed").is_empty());
    for directory in [CELLAR, BIN] {
        assert!(
            fs::read_dir(prefix.join(directory))
                .expect("managed directory")
                .next()
                .is_none(),
            "{directory} should be empty"
        );
    }
    assert!(
        fs::read_dir(prefix.join(META).join(STAGES))
            .expect("stages")
            .next()
            .is_none()
    );
    assert!(prefix.join(MARKER_FILE).is_file());
}

#[test]
fn removal_honours_the_install_writer_lock() {
    let (_temp, store) = new_store();
    let hello = install(&store, "hello", "1.0", &[("hello", true)]);
    let prefix = store.prefix().to_path_buf();
    let lock = prefix.join(META).join(STATE).join(LOCK_FILE);
    fs::write(&lock, b"pid=1 nonce=stale\n").expect("write stale lock");
    let before = tree(&prefix);

    let error = read_only(&store)
        .remove(std::slice::from_ref(&hello))
        .expect_err("a held lock must fail");
    assert!(error_chain(&error).contains("stale"), "{error:#}");
    assert_eq!(tree(&prefix), before);
}

#[test]
fn removal_removes_a_dangling_owned_link_and_allows_reinstall() {
    let (_temp, store) = new_store();
    let hello = install(&store, "hello", "1.0", &[("hello", true)]);
    let prefix = store.prefix().to_path_buf();
    let link = prefix.join(BIN).join("hello");
    fs::remove_file(
        prefix
            .join(CELLAR)
            .join("hello")
            .join("1.0")
            .join(BIN)
            .join("hello"),
    )
    .expect("delete the keg member");
    assert!(
        fs::symlink_metadata(&link)
            .expect("owned link")
            .file_type()
            .is_symlink()
    );

    read_only(&store)
        .remove(std::slice::from_ref(&hello))
        .expect("remove the dangling link");

    assert!(fs::symlink_metadata(&link).is_err());
    assert!(read_only(&store).installed().expect("installed").is_empty());
    // A reinstall must not collide with the old dangling link.
    install(&store, "hello", "1.0", &[("hello", true)]);
    assert_eq!(read_only(&store).installed().expect("installed").len(), 1);
    assert_eq!(
        fs::read_link(prefix.join(BIN).join("hello")).expect("new link"),
        Path::new("../Cellar/hello/1.0/bin/hello")
    );
}

#[test]
fn removal_rolls_back_every_move_when_apply_fails() {
    let (_temp, store) = new_store();
    let hello = install(
        &store,
        "hello",
        "1.0",
        &[("hello", true), ("hello-tool", true)],
    );
    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    // Steps are receipt, each link (bin + opt), then the keg; failing before
    // the keg makes the rollback restore several paths.
    let error = store
        .remove_inner(std::slice::from_ref(&hello), Some(4))
        .expect_err("injected removal failure");
    assert!(error_chain(&error).contains("injected"), "{error:#}");
    assert_eq!(tree(&prefix), before);
    assert_eq!(
        read_only(&store).installed().expect("installed"),
        vec![hello]
    );
}

#[test]
fn removal_rolls_back_a_pruned_package_directory() {
    let (_temp, store) = new_store();
    let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
    let beta = install(&store, "beta", "1.0", &[("beta", true)]);
    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    // Alpha's steps are receipt, bin link, opt link, keg, prune; beta then
    // moves its receipt. Failing at beta's link forces alpha's pruned
    // directory to be recreated before alpha's keg can return.
    let error = store
        .remove_inner(&[alpha, beta], Some(6))
        .expect_err("injected removal failure");
    assert!(error_chain(&error).contains("injected"), "{error:#}");
    assert_eq!(tree(&prefix), before);
    assert_eq!(read_only(&store).installed().expect("installed").len(), 2);
}

#[test]
fn removal_keeps_foreign_cellar_entries_and_the_marker() {
    let (_temp, store) = new_store();
    let hello = install(&store, "hello", "1.0", &[("hello", true)]);
    let prefix = store.prefix().to_path_buf();
    let foreign = prefix.join(CELLAR).join("hello").join("1.0-notes");
    fs::create_dir(&foreign).expect("foreign cellar entry");
    fs::write(foreign.join("keep"), b"keep").expect("foreign payload");

    read_only(&store)
        .remove(std::slice::from_ref(&hello))
        .expect("remove hello");

    assert_eq!(fs::read(foreign.join("keep")).expect("keep"), b"keep");
    assert!(prefix.join(CELLAR).join("hello").is_dir());
    assert!(!prefix.join(CELLAR).join("hello").join("1.0").exists());
    assert_eq!(
        fs::read(prefix.join(MARKER_FILE)).expect("marker"),
        MARKER_CONTENT.as_bytes()
    );
    assert!(read_only(&store).installed().expect("installed").is_empty());
}

#[test]
fn removal_requires_a_marker_and_never_initializes_a_prefix() {
    let temp = tempdir().expect("temporary directory");
    let target = receipt("hello", "1.0", true);

    let absent = temp.path().join("absent");
    let error = Store::open(&absent, false)
        .expect("bind absent prefix")
        .remove(std::slice::from_ref(&target))
        .expect_err("absent prefix must fail");
    assert!(
        error_chain(&error).contains("not an initialized"),
        "{error:#}"
    );
    assert!(!absent.exists());

    let unowned = temp.path().join("unowned");
    fs::create_dir(&unowned).expect("create unowned prefix");
    fs::write(unowned.join("keep"), b"keep").expect("foreign file");
    let error = Store::open(&unowned, false)
        .expect("bind unowned prefix")
        .remove(&[target])
        .expect_err("unowned prefix must fail");
    assert!(
        error_chain(&error).contains("not an initialized"),
        "{error:#}"
    );
    assert_eq!(fs::read(unowned.join("keep")).expect("keep"), b"keep");
    assert!(!unowned.join(MARKER_FILE).exists());
    assert!(!unowned.join(META).exists());
}

#[test]
fn removal_refuses_a_prefix_inside_homebrew() {
    let candidate =
        Path::new("/usr/local").join(format!("brewrs-remove-test-{}", std::process::id()));
    let store = Store {
        prefix: candidate.clone(),
        state: RootState::Owned,
        writable: false,
    };
    let error = store
        .remove(&[receipt("hello", "1.0", true)])
        .expect_err("Homebrew prefix must be refused");
    assert!(error_chain(&error).contains("Homebrew"), "{error:#}");
    assert!(!candidate.exists());
}

#[test]
fn open_existing_requires_ownership_and_never_initializes() {
    let temp = tempdir().expect("temporary directory");

    let absent = temp.path().join("absent");
    let error = Store::open_existing(&absent).expect_err("absent prefix must fail");
    assert!(
        error_chain(&error).contains("not an initialized"),
        "{error:#}"
    );
    assert!(!absent.exists());

    let unowned = temp.path().join("unowned");
    fs::create_dir(&unowned).expect("create unowned prefix");
    fs::write(unowned.join("keep"), b"keep").expect("foreign file");
    let error = Store::open_existing(&unowned).expect_err("unowned prefix must fail");
    assert!(
        error_chain(&error).contains("not an initialized"),
        "{error:#}"
    );
    assert_eq!(fs::read(unowned.join("keep")).expect("keep"), b"keep");
    assert!(!unowned.join(MARKER_FILE).exists());
    assert!(!unowned.join(META).exists());

    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();
    let writable = Store::open_existing(&prefix).expect("writable binding");
    drop(store);
    writable.stage().expect("staging needs a writable binding");

    fs::remove_dir(prefix.join(BIN)).expect("remove managed directory");
    let error = Store::open_existing(&prefix).expect_err("missing directory must fail");
    assert!(error_chain(&error).contains("missing"), "{error:#}");
    assert!(!prefix.join(BIN).exists());
}

#[test]
fn upgrade_replaces_the_old_receipt_keg_and_owned_links() {
    let (_temp, store) = new_store();
    let old = install(
        &store,
        "hello",
        "1.0",
        &[("hello", true), ("hello-tool", true)],
    );
    let prefix = store.prefix().to_path_buf();
    fs::write(prefix.join("keep.txt"), b"keep").expect("foreign file");
    fs::write(prefix.join(BIN).join("note"), b"keep").expect("foreign bin file");

    let replacement = prepare(
        &store,
        "hello",
        "2.0",
        &[("hello", true), ("hello-next", true)],
    );
    let expected = replacement.receipt.clone();
    let writable = Store::open_existing(&prefix).expect("writable binding");
    writable
        .upgrade(vec![(old, replacement)])
        .expect("upgrade hello");

    assert_eq!(writable.installed().expect("installed"), vec![expected]);
    assert!(
        !prefix.join(CELLAR).join("hello").join("1.0").exists(),
        "the old keg must be gone"
    );
    assert_eq!(
        fs::read(
            prefix
                .join(CELLAR)
                .join("hello")
                .join("2.0")
                .join("lib")
                .join("hello.dylib")
        )
        .expect("new keg payload"),
        b"payload"
    );
    assert_eq!(
        fs::read_link(prefix.join(BIN).join("hello")).expect("new link"),
        Path::new("../Cellar/hello/2.0/bin/hello")
    );
    assert!(
        fs::symlink_metadata(prefix.join(BIN).join("hello-tool")).is_err(),
        "an old owned link must not survive"
    );
    assert_eq!(
        fs::read_link(prefix.join(BIN).join("hello-next")).expect("new tool"),
        Path::new("../Cellar/hello/2.0/bin/hello-next")
    );
    assert_eq!(fs::read(prefix.join("keep.txt")).expect("keep"), b"keep");
    assert_eq!(
        fs::read(prefix.join(BIN).join("note")).expect("keep bin"),
        b"keep"
    );
    assert!(
        fs::read_dir(prefix.join(META).join(STAGES))
            .expect("stages")
            .next()
            .is_none(),
        "a successful upgrade leaves no quarantine"
    );
    assert!(!prefix.join(META).join(STATE).join(LOCK_FILE).exists());
}

#[test]
fn upgrade_rejects_a_stale_or_missing_old_receipt() {
    let (_temp, store) = new_store();
    let old = install(&store, "hello", "1.0", &[("hello", true)]);
    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    let mut stale = old.clone();
    stale.version = "0.9".into();
    let error = store
        .upgrade(vec![(
            stale,
            prepare(&store, "hello", "2.0", &[("hello", true)]),
        )])
        .expect_err("stale old receipt must fail");
    assert!(error_chain(&error).contains("re-read"), "{error:#}");
    assert_eq!(tree(&prefix), before);

    let receipt_path = prefix
        .join(META)
        .join(STATE)
        .join(RECEIPTS)
        .join("hello.json");
    fs::remove_file(&receipt_path).expect("remove the receipt");
    let missing = tree(&prefix);
    let error = store
        .upgrade(vec![(
            old,
            prepare(&store, "hello", "2.0", &[("hello", true)]),
        )])
        .expect_err("missing old receipt must fail");
    assert!(
        error_chain(&error).contains("no longer exists"),
        "{error:#}"
    );
    assert_eq!(tree(&prefix), missing);
}

#[test]
fn upgrade_requires_matching_name_requested_status_and_a_new_version() {
    let (_temp, store) = new_store();
    let old = install(&store, "hello", "1.0", &[("hello", true)]);
    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    let mut renamed = prepare(&store, "hello", "2.0", &[("hello", true)]);
    renamed.receipt.name = "other".into();
    let error = store
        .upgrade(vec![(old.clone(), renamed)])
        .expect_err("name mismatch must fail");
    assert!(error_chain(&error).contains("staged package"), "{error:#}");
    assert_eq!(tree(&prefix), before);

    let mut unrequested = prepare(&store, "hello", "2.0", &[("hello", true)]);
    unrequested.receipt.requested = false;
    let error = store
        .upgrade(vec![(old.clone(), unrequested)])
        .expect_err("requested mismatch must fail");
    assert!(
        error_chain(&error).contains("requested status"),
        "{error:#}"
    );
    assert_eq!(tree(&prefix), before);

    let error = store
        .upgrade(vec![(
            old,
            prepare(&store, "hello", "1.0", &[("hello", true)]),
        )])
        .expect_err("same version must fail");
    assert!(
        error_chain(&error).contains("already installed"),
        "{error:#}"
    );
    assert_eq!(tree(&prefix), before);
}

#[test]
fn upgrade_rejects_foreign_alternate_version_and_batch_collisions() {
    let (_temp, store) = new_store();
    let alpha = install(&store, "alpha", "1.0", &[("alpha", true)]);
    let beta = install(&store, "beta", "1.0", &[("beta", true)]);
    let prefix = store.prefix().to_path_buf();

    let foreign = prefix.join(BIN).join("tool");
    fs::write(&foreign, b"keep").expect("foreign bin file");
    let before = tree(&prefix);
    let error = store
        .upgrade(vec![(
            alpha.clone(),
            prepare(&store, "alpha", "2.0", &[("tool", true)]),
        )])
        .expect_err("foreign bin collision must fail");
    assert!(error_chain(&error).contains("already exists"), "{error:#}");
    assert_eq!(tree(&prefix), before);
    assert_eq!(fs::read(&foreign).expect("foreign"), b"keep");

    let error = store
        .upgrade(vec![
            (
                alpha.clone(),
                prepare(&store, "alpha", "2.0", &[("shared", true)]),
            ),
            (
                beta.clone(),
                prepare(&store, "beta", "2.0", &[("shared", true)]),
            ),
        ])
        .expect_err("batch bin collision must fail");
    assert!(
        error_chain(&error).contains("more than one package"),
        "{error:#}"
    );
    assert_eq!(tree(&prefix), before);

    let error = store
        .upgrade(vec![
            (
                alpha.clone(),
                prepare(&store, "alpha", "2.0", &[("alpha", true)]),
            ),
            (
                alpha.clone(),
                prepare(&store, "alpha", "3.0", &[("alpha", true)]),
            ),
        ])
        .expect_err("duplicate package name must fail");
    assert!(error_chain(&error).contains("more than once"), "{error:#}");
    assert_eq!(tree(&prefix), before);

    let alternate = prefix.join(CELLAR).join("alpha").join("0.9");
    fs::create_dir(&alternate).expect("alternate version");
    let damaged = tree(&prefix);
    let error = store
        .upgrade(vec![(
            alpha,
            prepare(&store, "alpha", "2.0", &[("alpha", true)]),
        )])
        .expect_err("alternate version must fail");
    assert!(
        error_chain(&error).contains("not the installed version"),
        "{error:#}"
    );
    assert_eq!(tree(&prefix), damaged);
    assert!(alternate.is_dir());
}

#[test]
fn upgrade_rolls_back_new_paths_and_restores_the_old_package() {
    let (_temp, store) = new_store();
    let alpha = install(
        &store,
        "alpha",
        "1.0",
        &[("alpha", true), ("alpha-tool", true)],
    );
    let beta = install(&store, "beta", "1.0", &[("beta", true)]);
    let prefix = store.prefix().to_path_buf();
    fs::write(prefix.join("keep.txt"), b"keep").expect("foreign file");
    let before = tree(&prefix);

    let first = prepare(
        &store,
        "alpha",
        "2.0",
        &[("alpha", true), ("alpha-next", true)],
    );
    let second = prepare(&store, "beta", "2.0", &[("beta", true)]);
    let error = store
        .upgrade_inner(
            vec![(alpha.clone(), first), (beta.clone(), second)],
            Some(1),
        )
        .expect_err("injected upgrade failure");
    assert!(error_chain(&error).contains("injected"), "{error:#}");

    assert_eq!(tree(&prefix), before);
    assert_eq!(
        read_only(&store).installed().expect("installed"),
        vec![alpha, beta]
    );
    assert!(
        fs::read_dir(prefix.join(META).join(STAGES))
            .expect("stages")
            .next()
            .is_none(),
        "a complete rollback drops the quarantine"
    );
    assert!(!prefix.join(META).join(STATE).join(LOCK_FILE).exists());
}

#[test]
fn receipt_without_source_deserializes_as_homebrew() {
    let old = serde_json::json!({
        "name": "legacy",
        "version": "1.0",
        "sha256": digest(),
        "requested": true,
    });
    let receipt: Receipt = serde_json::from_value(old).expect("old receipt");
    assert_eq!(receipt.source, PackageSource::Homebrew);
    let mut upstream = receipt;
    upstream.source = PackageSource::Upstream;
    let text = serde_json::to_string(&upstream).unwrap();
    assert!(text.contains("\"source\":\"upstream\""));
    let round: Receipt = serde_json::from_str(&text).unwrap();
    assert_eq!(round.source, PackageSource::Upstream);
}

#[test]
fn receipts_written_without_dependencies_still_load_and_are_satisfiable() {
    let (_temp, store) = new_store();
    let legacy = serde_json::json!({
        "name": "legacy",
        "version": "1.0",
        "sha256": digest(),
        "requested": true,
    })
    .to_string();
    fs::write(
        receipts_dir(store.prefix()).join("legacy.json"),
        legacy.as_bytes(),
    )
    .expect("write a pre-dependency receipt");

    let installed = store.installed().expect("installed");
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].name, "legacy");
    assert!(installed[0].dependencies.is_empty());

    let dependent = prepare_with(
        &store,
        "dependent",
        "1.0",
        &[("dependent", true)],
        &[dependency(&installed[0])],
    );
    store
        .commit(vec![dependent])
        .expect("a dependent may pin a legacy receipt");
    assert_eq!(store.installed().expect("installed").len(), 2);
}

#[test]
fn commit_installs_a_dependency_graph_transactionally() {
    let (_temp, store) = new_store();
    let libfoo = prepare(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let libfoo_receipt = libfoo.receipt.clone();
    let app = prepare_with(
        &store,
        "app",
        "1.0",
        &[("app", true)],
        &[dependency(&libfoo_receipt)],
    );
    store
        .commit(vec![libfoo, app])
        .expect("install a coherent dependency graph");

    let installed = store.installed().expect("installed");
    assert_eq!(installed.len(), 2);
    assert_eq!(installed[0].name, "app");
    assert_eq!(installed[0].dependencies, vec![dependency(&installed[1])]);

    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    // A version that is not installed.
    let mut missing_version = prepare(&store, "tool", "1.0", &[("tool", true)]);
    missing_version.receipt.dependencies = vec![Dependency {
        name: "libfoo".into(),
        version: "2.0".into(),
        sha256: digest(),
    }];
    let error = store
        .commit(vec![missing_version])
        .expect_err("a missing dependency version must fail");
    let message = error_chain(&error);
    assert!(message.contains("'tool'"), "{message}");
    assert!(message.contains("libfoo 2.0"), "{message}");
    assert_eq!(tree(&prefix), before);

    // The recorded version with a different digest is a different pin.
    let mut wrong_digest = prepare(&store, "other", "1.0", &[("other", true)]);
    wrong_digest.receipt.dependencies = vec![Dependency {
        name: "libfoo".into(),
        version: "1.0".into(),
        sha256: "b".repeat(64),
    }];
    let error = store
        .commit(vec![wrong_digest])
        .expect_err("a mismatched dependency digest must fail");
    let message = error_chain(&error);
    assert!(message.contains("'other'"), "{message}");
    assert!(message.contains("libfoo 1.0"), "{message}");
    assert_eq!(tree(&prefix), before);

    // One dangling edge rejects the whole batch before any package applies.
    let good = prepare(&store, "good", "1.0", &[("good", true)]);
    let mut bad = prepare(&store, "bad", "1.0", &[("bad", true)]);
    bad.receipt.dependencies = vec![Dependency {
        name: "absent".into(),
        version: "1.0".into(),
        sha256: digest(),
    }];
    let error = store
        .commit(vec![good, bad])
        .expect_err("a dangling edge must fail the batch");
    assert!(error_chain(&error).contains("'bad'"), "{error:#}");
    assert_eq!(tree(&prefix), before);
    assert_eq!(store.installed().expect("installed").len(), 2);
    assert!(!prefix.join(CELLAR).join("good").exists());
    assert!(!prefix.join(CELLAR).join("bad").exists());
}

#[test]
fn commit_with_promotions_promotes_and_installs_in_one_transaction() {
    let (_temp, store) = new_store();
    let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
    let original_bytes = fs::read(&receipt_path).expect("original receipt");
    let link = store.prefix().join(BIN).join("libfoo");
    let link_target = fs::read_link(&link).expect("existing link");

    let app = prepare_with(
        &store,
        "app",
        "1.0",
        &[("app", true)],
        &[dependency(&libfoo)],
    );
    let app_receipt = app.receipt.clone();
    store
        .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
        .expect("install app and promote libfoo");

    let installed = store.installed().expect("installed");
    let promoted = installed
        .iter()
        .find(|receipt| receipt.name == "libfoo")
        .expect("libfoo stays installed");
    let mut expected = libfoo.clone();
    expected.requested = true;
    assert_eq!(*promoted, expected, "only the requested flag changes");
    assert!(
        installed.contains(&app_receipt),
        "the new package is installed"
    );
    assert_eq!(
        fs::read(&receipt_path).expect("promoted receipt"),
        encode_receipt(&expected).expect("encode promoted receipt")
    );
    assert_ne!(
        fs::read(&receipt_path).expect("promoted receipt"),
        original_bytes
    );
    assert_eq!(
        fs::read_link(&link).expect("link survives promotion"),
        link_target
    );
    assert!(
        store
            .prefix()
            .join(CELLAR)
            .join("libfoo")
            .join("1.0")
            .is_dir(),
        "the dependency keg is untouched"
    );
    assert!(
        !store
            .prefix()
            .join(META)
            .join(STATE)
            .join(LOCK_FILE)
            .exists()
    );
}

#[test]
fn commit_with_promotions_promotes_without_new_packages() {
    let (_temp, store) = new_store();
    let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);

    store
        .commit_with_promotions(Vec::new(), std::slice::from_ref(&libfoo))
        .expect("promote an installed dependency");

    let installed = store.installed().expect("installed");
    assert_eq!(installed.len(), 1);
    assert!(installed[0].requested);
    assert_eq!(installed[0].name, "libfoo");
}

#[test]
fn commit_with_promotions_without_expected_delegates_to_commit() {
    let (_temp, store) = new_store();
    let prepared = prepare(&store, "hello", "1.0", &[("hello", true)]);
    let expected = prepared.receipt.clone();

    store
        .commit_with_promotions(vec![prepared], &[])
        .expect("plain commit");
    assert_eq!(store.installed().expect("installed"), vec![expected]);

    store
        .commit_with_promotions(Vec::new(), &[])
        .expect("empty commit is a no-op");

    let error = store
        .commit_with_promotions(vec![prepare(&store, "hello", "1.0", &[])], &[])
        .expect_err("the delegated commit still rejects an installed package");
    assert!(
        error_chain(&error).contains("already installed"),
        "{error:#}"
    );
    assert_eq!(store.installed().expect("installed").len(), 1);
}

#[test]
fn promotion_refuses_stale_missing_or_changed_receipts() {
    let (_temp, store) = new_store();
    let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let path = receipts_dir(store.prefix()).join("libfoo.json");

    let changes: [fn(&mut Receipt); 3] = [
        |receipt| receipt.requested = true,
        |receipt| receipt.sha256 = "b".repeat(64),
        |receipt| {
            receipt.dependencies = vec![Dependency {
                name: "pin".into(),
                version: "9.9".into(),
                sha256: digest(),
            }]
        },
    ];
    for change in changes {
        let mut changed = libfoo.clone();
        change(&mut changed);
        let bytes = encode_receipt(&changed).expect("encode changed receipt");
        replace_receipt(&path, &bytes).expect("rewrite the receipt");

        let before = tree(store.prefix());
        let app = prepare(&store, "app", "1.0", &[("app", true)]);
        let error = store
            .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
            .expect_err("a changed receipt must be refused");
        let message = error_chain(&error);
        assert!(message.contains("re-read"), "{message}");
        assert_eq!(tree(store.prefix()), before);
        assert_eq!(fs::read(&path).expect("changed receipt"), bytes);
        assert!(
            store
                .installed()
                .expect("installed")
                .iter()
                .all(|receipt| receipt.name != "app")
        );
    }

    fs::remove_file(&path).expect("remove the receipt");
    let before = tree(store.prefix());
    let app = prepare(&store, "app", "1.0", &[("app", true)]);
    let error = store
        .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
        .expect_err("a missing receipt must be refused");
    assert!(
        error_chain(&error).contains("could not be re-read"),
        "{error:#}"
    );
    assert_eq!(tree(store.prefix()), before);
}

#[test]
fn promotion_requires_an_owned_real_keg() {
    use std::os::unix::fs::symlink;

    let (_temp, store) = new_store();
    let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let keg = store.prefix().join(CELLAR).join("libfoo").join("1.0");
    let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
    let original_bytes = fs::read(&receipt_path).expect("original receipt");
    let outside = tempdir().expect("outside directory");
    let parked = outside.path().join("keg");
    fs::rename(&keg, &parked).expect("park the keg");

    let app = prepare(&store, "app", "1.0", &[("app", true)]);
    let error = store
        .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
        .expect_err("a missing keg must be refused");
    assert!(error_chain(&error).contains("keg"), "{error:#}");
    assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
    assert!(
        store
            .installed()
            .expect("installed")
            .iter()
            .all(|receipt| receipt.name != "app")
    );

    symlink(&parked, &keg).expect("replace the keg with a symlink");
    let app = prepare(&store, "app", "1.0", &[("app", true)]);
    let error = store
        .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
        .expect_err("a symlinked keg must be refused");
    assert!(error_chain(&error).contains("symlink"), "{error:#}");
    assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
    assert!(!store.prefix().join(CELLAR).join("app").exists());
}

#[test]
fn promotion_rejects_duplicates_overlap_and_already_requested_receipts() {
    let (_temp, store) = new_store();
    let alpha = install_unrequested(&store, "alpha", "1.0", &[("alpha", true)]);
    let requested = install(&store, "gamma", "1.0", &[("gamma", true)]);
    let before = tree(store.prefix());

    let error = store
        .commit_with_promotions(Vec::new(), &[alpha.clone(), alpha.clone()])
        .expect_err("a duplicate promotion must be refused");
    assert!(error_chain(&error).contains("more than once"), "{error:#}");
    assert_eq!(tree(store.prefix()), before);

    let overlapping = prepare(&store, "alpha", "2.0", &[("alpha-next", true)]);
    let error = store
        .commit_with_promotions(vec![overlapping], std::slice::from_ref(&alpha))
        .expect_err("an install/promotion overlap must be refused");
    assert!(
        error_chain(&error).contains("install and promote"),
        "{error:#}"
    );
    assert_eq!(tree(store.prefix()), before);

    let error = store
        .commit_with_promotions(Vec::new(), std::slice::from_ref(&requested))
        .expect_err("an already requested receipt must be refused");
    assert!(
        error_chain(&error).contains("already requested"),
        "{error:#}"
    );
    assert_eq!(tree(store.prefix()), before);
}

#[test]
fn promotion_rolls_back_new_paths_and_restores_promoted_receipt_bytes() {
    let (_temp, store) = new_store();
    let alpha = install_unrequested(&store, "alpha", "1.0", &[("alpha", true)]);
    let beta = install_unrequested(&store, "beta", "1.0", &[("beta", true)]);
    let alpha_path = receipts_dir(store.prefix()).join("alpha.json");
    let beta_path = receipts_dir(store.prefix()).join("beta.json");
    let alpha_bytes = fs::read(&alpha_path).expect("alpha receipt");
    let beta_bytes = fs::read(&beta_path).expect("beta receipt");
    let before = tree(store.prefix());

    let app = prepare(&store, "app", "1.0", &[("app", true)]);
    let error = store
        .commit_with_promotions_inner(vec![app], &[alpha, beta], None, Some(1))
        .expect_err("injected promotion failure");

    assert!(
        error_chain(&error).contains("injected promotion failure"),
        "{error:#}"
    );
    assert_eq!(
        tree(store.prefix()),
        before,
        "new installs and the first promoted receipt are undone"
    );
    assert_eq!(fs::read(&alpha_path).expect("alpha receipt"), alpha_bytes);
    assert_eq!(fs::read(&beta_path).expect("beta receipt"), beta_bytes);
    let installed = store.installed().expect("installed");
    assert!(installed.iter().all(|receipt| receipt.name != "app"));
    assert!(installed.iter().all(|receipt| !receipt.requested));
}

#[test]
fn promotion_keeps_receipts_when_a_new_install_fails() {
    let (_temp, store) = new_store();
    let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
    let original_bytes = fs::read(&receipt_path).expect("original receipt");
    let before = tree(store.prefix());

    let first = prepare(&store, "first", "1.0", &[("first", true)]);
    let second = prepare(&store, "second", "1.0", &[("second", true)]);
    let error = store
        .commit_with_promotions_inner(
            vec![first, second],
            std::slice::from_ref(&libfoo),
            Some(1),
            None,
        )
        .expect_err("injected install failure");

    assert!(
        error_chain(&error).contains("injected commit failure"),
        "{error:#}"
    );
    assert_eq!(tree(store.prefix()), before);
    assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
    assert!(
        !store
            .installed()
            .expect("installed")
            .iter()
            .any(|receipt| receipt.name == "libfoo" && receipt.requested)
    );
}

#[test]
fn promotion_validates_the_projected_graph_before_writing() {
    let (_temp, store) = new_store();
    let libfoo = install_unrequested(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let receipt_path = receipts_dir(store.prefix()).join("libfoo.json");
    let original_bytes = fs::read(&receipt_path).expect("original receipt");
    let before = tree(store.prefix());

    let mut app = prepare_with(
        &store,
        "app",
        "1.0",
        &[("app", true)],
        &[dependency(&libfoo)],
    );
    app.receipt.dependencies[0].sha256 = "b".repeat(64);
    let error = store
        .commit_with_promotions(vec![app], std::slice::from_ref(&libfoo))
        .expect_err("a mismatched projected pin must fail");

    assert!(
        error_chain(&error).contains("does not contain"),
        "{error:#}"
    );
    assert_eq!(tree(store.prefix()), before);
    assert_eq!(fs::read(&receipt_path).expect("receipt"), original_bytes);
    assert!(
        store
            .installed()
            .expect("installed")
            .iter()
            .all(|receipt| !receipt.requested)
    );
}

#[test]
fn removal_blocks_unselected_dependents_and_allows_a_batch() {
    let (_temp, store) = new_store();
    let libfoo = install(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let app = install_with_dependency(&store, "app", "1.0", &libfoo, &[("app", true)]);
    let prefix = store.prefix().to_path_buf();
    let before = tree(&prefix);

    let error = read_only(&store)
        .remove(std::slice::from_ref(&libfoo))
        .expect_err("an installed dependent must block removal");
    let message = error_chain(&error);
    assert!(message.contains("'app'"), "{message}");
    assert!(message.contains("libfoo 1.0"), "{message}");
    assert_eq!(tree(&prefix), before);
    assert_eq!(read_only(&store).installed().expect("installed").len(), 2);

    read_only(&store)
        .remove(&[libfoo, app])
        .expect("a batch that removes the dependent too");
    assert!(read_only(&store).installed().expect("installed").is_empty());
    assert!(!prefix.join(CELLAR).join("libfoo").exists());
    assert!(!prefix.join(CELLAR).join("app").exists());
}

#[test]
fn upgrade_retains_a_pinned_old_keg_and_allows_coherent_batches() {
    let (_temp, store) = new_store();
    let libfoo = install(&store, "libfoo", "1.0", &[("libfoo", true)]);
    let app = install_with_dependency(&store, "app", "1.0", &libfoo, &[("app", true)]);
    let prefix = store.prefix().to_path_buf();

    // A surviving dependent pins the exact old version and digest, so the
    // upgrade proceeds and retains the old keg and identity instead of
    // releasing them.
    let breaking = prepare(&store, "libfoo", "2.0", &[("libfoo", true)]);
    store
        .upgrade(vec![(libfoo.clone(), breaking)])
        .expect("a pinned old version is retained, not rejected");
    let installed = store.installed().expect("installed");
    assert_eq!(installed.len(), 2);
    let app_receipt = installed.iter().find(|r| r.name == "app").expect("app");
    assert_eq!(app_receipt.dependencies[0].version, "1.0");
    let libfoo_receipt = installed
        .iter()
        .find(|r| r.name == "libfoo")
        .expect("libfoo")
        .clone();
    assert_eq!(libfoo_receipt.version, "2.0");

    // The old keg and its exact identity survive; the active links move.
    let retained_keg = prefix.join(CELLAR).join("libfoo").join("1.0");
    assert!(retained_keg.is_dir(), "the pinned old keg must be retained");
    assert!(prefix.join(CELLAR).join("libfoo").join("2.0").is_dir());
    assert_eq!(
        fs::read_link(prefix.join(BIN).join("libfoo")).expect("new link"),
        Path::new("../Cellar/libfoo/2.0/bin/libfoo")
    );
    let retained_record = prefix
        .join(META)
        .join(STATE)
        .join(RETAINED)
        .join("libfoo")
        .join("1.0.json");
    assert_eq!(
        fs::read(&retained_record).expect("retained record"),
        encode_receipt(&libfoo).expect("old receipt bytes")
    );
    assert!(
        fs::read_dir(prefix.join(META).join(STAGES))
            .expect("stages")
            .next()
            .is_none(),
        "a successful retention upgrade leaves no quarantine"
    );

    // A dependent moved onto a version that is not installed and not
    // retained still fails.
    let absent = prepare_with(
        &store,
        "app",
        "2.0",
        &[("app", true)],
        &[Dependency {
            name: "libfoo".into(),
            version: "9.0".into(),
            sha256: digest(),
        }],
    );
    let error = store
        .upgrade(vec![(app.clone(), absent)])
        .expect_err("an unresolvable new dependency must fail");
    assert!(error_chain(&error).contains("'app'"), "{error:#}");
    assert_eq!(store.installed().expect("installed").len(), 2);

    // Upgrading both sides in one call keeps the graph coherent.
    let libfoo_next = prepare(&store, "libfoo", "3.0", &[("libfoo", true)]);
    let libfoo_next_receipt = libfoo_next.receipt.clone();
    let app_next = prepare_with(
        &store,
        "app",
        "2.0",
        &[("app", true)],
        &[dependency(&libfoo_next_receipt)],
    );
    store
        .upgrade(vec![(libfoo_receipt, libfoo_next), (app, app_next)])
        .expect("a coherent batch upgrade");
    let installed = store.installed().expect("installed");
    assert_eq!(installed.len(), 2);
    assert_eq!(installed[0].name, "app");
    assert_eq!(installed[0].version, "2.0");
    assert_eq!(installed[0].dependencies[0].version, "3.0");
    assert_eq!(installed[1].version, "3.0");
    // The retained 1.0 keg is now unpinned, but garbage collection is
    // deferred: the keg and its record remain for a future milestone.
    assert!(retained_keg.is_dir());
    assert!(retained_record.is_file());
}

#[test]
fn dependency_validation_rejects_bad_pins_and_duplicate_graphs() {
    assert!(validate_dependencies(&[]).is_ok());

    let alpha = receipt("alpha", "1.0", true);
    let beta = receipt("beta", "1.0", true);
    assert!(validate_dependencies(std::slice::from_ref(&alpha)).is_ok());

    let mut pinned = alpha.clone();
    pinned.dependencies = vec![dependency(&beta)];
    assert!(validate_dependencies(&[pinned.clone(), beta.clone()]).is_ok());

    let error = validate_dependencies(std::slice::from_ref(&pinned))
        .expect_err("a missing dependency must fail");
    let message = error_chain(&error);
    assert!(message.contains("'alpha'"), "{message}");
    assert!(message.contains("'beta 1.0'"), "{message}");

    let mut itself = alpha.clone();
    itself.dependencies = vec![dependency(&alpha)];
    let error = validate_dependencies(&[itself]).expect_err("a self dependency must fail");
    assert!(error_chain(&error).contains("itself"), "{error:#}");

    let mut twice = alpha.clone();
    twice.dependencies = vec![dependency(&beta), dependency(&beta)];
    let error =
        validate_dependencies(&[twice, beta.clone()]).expect_err("a repeated dependency must fail");
    assert!(error_chain(&error).contains("more than once"), "{error:#}");

    let error = validate_dependencies(&[alpha.clone(), alpha.clone()])
        .expect_err("a duplicate package must fail");
    assert!(
        error_chain(&error).contains("projected receipt graph"),
        "{error:#}"
    );

    for bad_pin in [
        Dependency {
            name: "../evil".into(),
            version: "1.0".into(),
            sha256: digest(),
        },
        Dependency {
            name: "beta".into(),
            version: "../1".into(),
            sha256: digest(),
        },
        Dependency {
            name: "beta".into(),
            version: "1.0".into(),
            sha256: digest().to_uppercase(),
        },
        Dependency {
            name: "beta".into(),
            version: "1.0".into(),
            sha256: "a".repeat(63),
        },
    ] {
        let mut broken = alpha.clone();
        broken.dependencies = vec![bad_pin];
        assert!(validate_dependencies(&[broken]).is_err());
    }
}

#[test]
fn filters_safe_bin_names() {
    for safe in [
        "hello",
        "python3.11",
        "libsigc++",
        "openssl@3",
        "_tool",
        "g++-13",
    ] {
        assert!(is_safe_bin_name(safe), "{safe} should be safe");
    }
    for unsafe_name in [
        "", ".", "..", ".hidden", "../evil", "a/b", "a b", "-rf", "a\\b",
    ] {
        assert!(
            !is_safe_bin_name(unsafe_name),
            "{unsafe_name} should be unsafe"
        );
    }
    assert!(is_safe_package_name("openssl@3"));
    assert!(!is_safe_package_name("OpenSSL"));
    assert!(is_safe_version("1.0.0-rc1"));
    assert!(!is_safe_version("1.0/2"));
    assert!(is_lower_hex_digest(&digest()));
    assert!(!is_lower_hex_digest(&digest().to_uppercase()));
}

#[cfg(unix)]
#[test]
fn links_keg_contained_executable_bin_symlinks_and_rejects_escaping_ones() {
    use std::os::unix::fs::symlink;

    let (_temp, store) = new_store();
    let stage = store.stage().expect("create stage");
    let libexec_bin = stage.path().join("libexec/bin");
    let stage_bin = stage.path().join("bin");
    fs::create_dir_all(&libexec_bin).expect("libexec/bin");
    fs::create_dir_all(&stage_bin).expect("bin");

    let tool = libexec_bin.join("tool");
    fs::write(&tool, b"#!/bin/sh\n").expect("write tool");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("chmod tool");
    symlink("../libexec/bin/tool", stage_bin.join("tool")).expect("link tool");

    let doc = libexec_bin.join("doc");
    fs::write(&doc, b"readme").expect("write doc");
    fs::set_permissions(&doc, fs::Permissions::from_mode(0o644)).expect("chmod doc");
    symlink("../libexec/bin/doc", stage_bin.join("doc")).expect("link doc");

    let prepared = Prepared {
        receipt: receipt("go", "1.27.4", true),
        stage,
    };
    let installed_receipt = prepared.receipt.clone();
    store.commit(vec![prepared]).expect("commit keg symlink");

    let tool_link = store.prefix().join(BIN).join("tool");
    assert_eq!(
        fs::read_link(&tool_link).expect("read tool link"),
        Path::new("../Cellar/go/1.27.4/bin/tool")
    );
    assert!(fs::symlink_metadata(store.prefix().join(BIN).join("doc")).is_err());

    read_only(&store)
        .remove(std::slice::from_ref(&installed_receipt))
        .expect("remove go");
    assert!(fs::symlink_metadata(&tool_link).is_err());

    // Absolute and escaping symlinks inside bin/ must be rejected without touching the prefix.
    let outside = _temp.path().join("outside");
    fs::write(&outside, b"#!/bin/sh\n").expect("write outside");
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).expect("chmod outside");

    for (label, target) in [
        ("absolute", "/usr/bin/true"),
        ("escaping", "../../../../../outside"),
    ] {
        let bad_stage = store.stage().expect("bad stage");
        fs::create_dir_all(bad_stage.path().join("bin")).expect("bad bin");
        symlink(target, bad_stage.path().join("bin/bad")).expect("bad symlink");
        let bad = Prepared {
            receipt: receipt("bad", "1.0", true),
            stage: bad_stage,
        };
        let error = store.commit(vec![bad]).expect_err(label);
        let message = error_chain(&error);
        assert!(
            message.contains("absolute symlink")
                || message.contains("escapes the staged keg")
                || message.contains("broken symlink"),
            "{label}: {message}"
        );
        assert!(store.installed().expect("installed").is_empty());
    }
}

#[cfg(unix)]
#[test]
fn keg_only_package_creates_opt_link_without_bin_links_and_cleans_up() {
    use std::os::unix::fs::symlink;

    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();

    let mut first = prepare(&store, "m4", "1.4.20", &[("m4", true)]);
    first.receipt.keg_only = true;
    let first_receipt = first.receipt.clone();
    store.commit(vec![first]).expect("install keg-only m4");

    let opt_link = prefix.join(OPT).join("m4");
    assert_eq!(
        fs::read_link(&opt_link).expect("opt link"),
        Path::new("../Cellar/m4/1.4.20")
    );
    assert!(fs::symlink_metadata(prefix.join(BIN).join("m4")).is_err());

    // Foreign opt/<name> blocks upgrade without altering installed state.
    let mut second = prepare(&store, "m4", "1.4.21", &[("m4", true)]);
    second.receipt.keg_only = true;
    fs::remove_file(&opt_link).unwrap();
    symlink("/tmp/foreign", &opt_link).unwrap();
    let foreign_err = store
        .upgrade(vec![(first_receipt.clone(), second)])
        .expect_err("foreign opt link must refuse upgrade");
    assert!(
        error_chain(&foreign_err).contains("not a link owned by"),
        "{foreign_err:#}"
    );
    fs::remove_file(&opt_link).unwrap();
    symlink("../Cellar/m4/1.4.20", &opt_link).unwrap();

    // Upgrade flips keg_only true -> false and updates opt/m4 + links bin/m4.
    let upgraded = prepare(&store, "m4", "1.4.21", &[("m4", true)]);
    let upgraded_receipt = upgraded.receipt.clone();
    store
        .upgrade(vec![(first_receipt, upgraded)])
        .expect("upgrade m4");
    assert_eq!(
        fs::read_link(&opt_link).expect("updated opt link"),
        Path::new("../Cellar/m4/1.4.21")
    );
    assert_eq!(
        fs::read_link(prefix.join(BIN).join("m4")).expect("new bin link"),
        Path::new("../Cellar/m4/1.4.21/bin/m4")
    );

    read_only(&store)
        .remove(std::slice::from_ref(&upgraded_receipt))
        .expect("remove m4");
    assert!(fs::symlink_metadata(&opt_link).is_err());
    assert!(fs::symlink_metadata(prefix.join(BIN).join("m4")).is_err());
    assert!(store.installed().expect("installed").is_empty());
}

#[test]
fn validate_conflicts_rejects_symmetric_and_same_batch_conflicts() {
    let (_temp, store) = new_store();

    let mut alpha = receipt("alpha", "1.0", true);
    alpha.conflicts_with = vec!["beta".into()];
    let beta = receipt("beta", "1.0", true);
    let err = validate_conflicts(&[alpha.clone(), beta.clone()]).expect_err("conflict");
    assert!(error_chain(&err).contains("conflicts with"), "{err:#}");
    // Symmetric: conflict declared on second item still rejects.
    let err = validate_conflicts(&[beta, alpha]).expect_err("reverse conflict");
    assert!(error_chain(&err).contains("conflicts with"), "{err:#}");

    let mut first = prepare(&store, "first", "1.0", &[("first", true)]);
    first.receipt.conflicts_with = vec!["second".into()];
    let second = prepare(&store, "second", "1.0", &[("second", true)]);
    let err = store
        .commit(vec![first, second])
        .expect_err("same-batch conflict must fail commit");
    assert!(error_chain(&err).contains("conflicts with"), "{err:#}");
    assert!(store.installed().expect("installed").is_empty());

    let base = install(&store, "base", "1.0", &[("base", true)]);
    let mut challenger = prepare(&store, "challenger", "1.0", &[("challenger", true)]);
    challenger.receipt.conflicts_with = vec!["base".into()];
    let err = store
        .commit(vec![challenger])
        .expect_err("conflict with installed package must fail commit");
    assert!(error_chain(&err).contains("conflicts with"), "{err:#}");

    // Upgrading an existing package to a version that conflicts with another installed package fails.
    let other = install(&store, "other", "1.0", &[("other", true)]);
    let mut base_v2 = prepare(&store, "base", "2.0", &[("base", true)]);
    base_v2.receipt.conflicts_with = vec!["other".into()];
    let err = store
        .upgrade(vec![(base, base_v2)])
        .expect_err("upgrade introducing conflict must fail");
    assert!(error_chain(&err).contains("conflicts with"), "{err:#}");
    assert_eq!(store.installed().expect("installed").len(), 2);
    let _ = other;
}

#[cfg(unix)]
#[test]
fn links_upgrades_and_removes_sbin_executables_safely() {
    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();

    let add_sbin = |prepared: &Prepared, members: &[(&str, bool)]| {
        let sbin = prepared.stage.path().join(SBIN);
        fs::create_dir_all(&sbin).unwrap();
        for (name, exec) in members {
            let path = sbin.join(name);
            fs::write(&path, b"#!/bin/sh\n").unwrap();
            fs::set_permissions(
                &path,
                fs::Permissions::from_mode(if *exec { 0o755 } else { 0o644 }),
            )
            .unwrap();
        }
    };

    // Pre-existing store lacking `sbin/` opens cleanly and creates `sbin/` on install.
    fs::remove_dir(sbin_dir(&prefix)).unwrap();

    let v1 = prepare(&store, "daemon", "1.0", &[("cli", true)]);
    add_sbin(&v1, &[("daemond", true), ("notes", false)]);
    let v1_receipt = v1.receipt.clone();
    store.commit(vec![v1]).expect("commit bin+sbin");

    assert!(sbin_dir(&prefix).is_dir());
    assert_eq!(
        fs::read_link(prefix.join(BIN).join("cli")).unwrap(),
        Path::new("../Cellar/daemon/1.0/bin/cli")
    );
    assert_eq!(
        fs::read_link(prefix.join(SBIN).join("daemond")).unwrap(),
        Path::new("../Cellar/daemon/1.0/sbin/daemond")
    );
    assert!(fs::symlink_metadata(prefix.join(SBIN).join("notes")).is_err());

    // Preserve foreign file in sbin/, refuse collision with foreign sbin/ member.
    fs::write(prefix.join(SBIN).join("foreign"), b"keep").unwrap();
    let colliding = prepare(&store, "other", "1.0", &[]);
    add_sbin(&colliding, &[("foreign", true)]);
    assert!(store.commit(vec![colliding]).is_err());

    let v2_collision = prepare(&store, "daemon", "2.0", &[("cli", true)]);
    add_sbin(&v2_collision, &[("foreign", true)]);
    assert!(
        store
            .upgrade(vec![(v1_receipt.clone(), v2_collision)])
            .is_err()
    );

    let v2 = prepare(&store, "daemon", "2.0", &[("cli", true)]);
    add_sbin(&v2, &[("daemond", true), ("daemonctl", true)]);
    let v2_receipt = v2.receipt.clone();
    store
        .upgrade(vec![(v1_receipt, v2)])
        .expect("upgrade bin+sbin");

    assert_eq!(
        fs::read_link(prefix.join(SBIN).join("daemond")).unwrap(),
        Path::new("../Cellar/daemon/2.0/sbin/daemond")
    );
    assert_eq!(
        fs::read_link(prefix.join(SBIN).join("daemonctl")).unwrap(),
        Path::new("../Cellar/daemon/2.0/sbin/daemonctl")
    );
    assert_eq!(
        fs::read(prefix.join(SBIN).join("foreign")).unwrap(),
        b"keep"
    );

    read_only(&store)
        .remove(std::slice::from_ref(&v2_receipt))
        .expect("remove bin+sbin");
    assert!(fs::symlink_metadata(prefix.join(SBIN).join("daemond")).is_err());
    assert!(fs::symlink_metadata(prefix.join(SBIN).join("daemonctl")).is_err());
    assert_eq!(
        fs::read(prefix.join(SBIN).join("foreign")).unwrap(),
        b"keep"
    );

    // Keg-only formula skips both bin/ and sbin/, linking only opt/<name>.
    let mut ko = prepare(&store, "kodaemon", "1.0", &[("ko-bin", true)]);
    ko.receipt.keg_only = true;
    add_sbin(&ko, &[("ko-sbin", true)]);
    let ko_receipt = ko.receipt.clone();
    store.commit(vec![ko]).expect("commit keg-only sbin");
    assert_eq!(
        fs::read_link(prefix.join(OPT).join("kodaemon")).unwrap(),
        Path::new("../Cellar/kodaemon/1.0")
    );
    assert!(fs::symlink_metadata(prefix.join(BIN).join("ko-bin")).is_err());
    assert!(fs::symlink_metadata(prefix.join(SBIN).join("ko-sbin")).is_err());
    read_only(&store)
        .remove(std::slice::from_ref(&ko_receipt))
        .expect("remove keg-only");
}

#[cfg(unix)]
#[test]
fn links_manpages_and_completions_through_install_upgrade_and_remove() {
    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();
    let docs = [
        ("share/man/man1/tool.1", b"man1".as_slice()),
        ("share/man/man8/tool.8", b"man8".as_slice()),
        ("etc/bash_completion.d/tool", b"bash".as_slice()),
        ("share/zsh/site-functions/_tool", b"zsh".as_slice()),
        (
            "share/fish/vendor_completions.d/tool.fish",
            b"fish".as_slice(),
        ),
    ];

    let v1 = prepare(&store, "tool", "1.0", &[("tool", true)]);
    for (relative, bytes) in docs {
        stage_file(&v1, relative, bytes, 0o644);
    }
    // A directory and a hidden member are neither direct nor safe members.
    fs::create_dir_all(v1.stage.path().join("share/man/man1/tool.d")).unwrap();
    stage_file(&v1, "share/man/man1/.hidden", b"hidden", 0o644);
    let v1_receipt = v1.receipt.clone();
    store.commit(vec![v1]).expect("install documentation links");

    assert_eq!(
        fs::read_link(prefix.join("share/man/man1/tool.1")).unwrap(),
        Path::new("../../../Cellar/tool/1.0/share/man/man1/tool.1")
    );
    assert_eq!(
        fs::read_link(prefix.join("etc/bash_completion.d/tool")).unwrap(),
        Path::new("../../Cellar/tool/1.0/etc/bash_completion.d/tool")
    );
    for (relative, _) in docs {
        assert_eq!(
            fs::read_link(prefix.join(relative)).unwrap(),
            keg_target("tool", "1.0", relative),
            "{relative}"
        );
    }
    assert!(fs::symlink_metadata(prefix.join("share/man/man1/tool.d")).is_err());
    assert!(fs::symlink_metadata(prefix.join("share/man/man1/.hidden")).is_err());

    // Upgrade replaces the man page and bash completion, switches the zsh
    // function to a keg-contained symlink, and drops the fish completion.
    let v2 = prepare(&store, "tool", "2.0", &[("tool", true)]);
    stage_file(&v2, "share/man/man1/tool.1", b"man1-v2", 0o644);
    stage_file(&v2, "share/zsh/site-functions/_tool.real", b"zsh-v2", 0o644);
    stage_link(&v2, "share/zsh/site-functions/_tool", "_tool.real");
    stage_file(&v2, "etc/bash_completion.d/tool", b"bash-v2", 0o644);
    let v2_receipt = v2.receipt.clone();
    store
        .upgrade(vec![(v1_receipt, v2)])
        .expect("upgrade documentation links");

    assert_eq!(
        fs::read(prefix.join("share/man/man1/tool.1")).unwrap(),
        b"man1-v2"
    );
    assert_eq!(
        fs::read_link(prefix.join("share/zsh/site-functions/_tool")).unwrap(),
        Path::new("../../../Cellar/tool/2.0/share/zsh/site-functions/_tool")
    );
    assert_eq!(
        fs::read(prefix.join("share/zsh/site-functions/_tool")).unwrap(),
        b"zsh-v2"
    );
    assert_eq!(
        fs::read(prefix.join("etc/bash_completion.d/tool")).unwrap(),
        b"bash-v2"
    );
    assert!(
        fs::symlink_metadata(prefix.join("share/fish/vendor_completions.d/tool.fish")).is_err()
    );

    // Removal recognizes an owned link by its exact target even after the
    // keg member it points at has disappeared.
    fs::remove_file(
        prefix
            .join(CELLAR)
            .join("tool")
            .join("2.0")
            .join("share/man/man1/tool.1"),
    )
    .unwrap();
    read_only(&store)
        .remove(std::slice::from_ref(&v2_receipt))
        .expect("remove documentation links");
    for (relative, _) in docs {
        assert!(
            fs::symlink_metadata(prefix.join(relative)).is_err(),
            "{relative}"
        );
    }
    assert!(store.installed().expect("installed").is_empty());

    // Keg-only packages never create these links, only opt/<name>.
    let mut keg_only = prepare(&store, "kegtool", "1.0", &[("kegtool", true)]);
    keg_only.receipt.keg_only = true;
    stage_file(&keg_only, "share/man/man1/kegtool.1", b"man", 0o644);
    stage_file(&keg_only, "etc/bash_completion.d/kegtool", b"bash", 0o644);
    let keg_only_receipt = keg_only.receipt.clone();
    store
        .commit(vec![keg_only])
        .expect("install keg-only package");
    assert!(fs::symlink_metadata(prefix.join("share/man/man1/kegtool.1")).is_err());
    assert!(fs::symlink_metadata(prefix.join("etc/bash_completion.d/kegtool")).is_err());
    assert_eq!(
        fs::read_link(prefix.join(OPT).join("kegtool")).unwrap(),
        Path::new("../Cellar/kegtool/1.0")
    );
    read_only(&store)
        .remove(std::slice::from_ref(&keg_only_receipt))
        .expect("remove keg-only package");
}

#[cfg(unix)]
#[test]
fn links_executable_docker_cli_plugins_and_ignores_other_members() {
    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();

    let v1 = prepare(&store, "docker", "27.0", &[("docker", true)]);
    stage_file(&v1, "bin/docker-buildx", b"#!/bin/sh\n", 0o755);
    stage_file(&v1, "bin/docker-compose", b"#!/bin/sh\n", 0o755);
    stage_link(
        &v1,
        "lib/docker/cli-plugins/docker-buildx",
        "../../../bin/docker-buildx",
    );
    stage_link(
        &v1,
        "lib/docker/cli-plugins/docker-compose",
        "../../../bin/docker-compose",
    );
    // Non-executable, unprefixed, and directory members are not plugins.
    stage_file(
        &v1,
        "lib/docker/cli-plugins/docker-helper",
        b"#!/bin/sh\n",
        0o644,
    );
    stage_file(&v1, "lib/docker/cli-plugins/other", b"#!/bin/sh\n", 0o755);
    fs::create_dir_all(v1.stage.path().join("lib/docker/cli-plugins/docker-dir")).unwrap();
    let v1_receipt = v1.receipt.clone();
    store.commit(vec![v1]).expect("install docker plugins");

    let plugins = prefix.join("lib/docker/cli-plugins");
    assert_eq!(
        fs::read_link(plugins.join("docker-buildx")).unwrap(),
        Path::new("../../../Cellar/docker/27.0/lib/docker/cli-plugins/docker-buildx")
    );
    assert_eq!(
        fs::read(plugins.join("docker-compose")).unwrap(),
        b"#!/bin/sh\n"
    );
    assert!(fs::symlink_metadata(plugins.join("docker-helper")).is_err());
    assert!(fs::symlink_metadata(plugins.join("other")).is_err());
    assert!(fs::symlink_metadata(plugins.join("docker-dir")).is_err());

    // An upgrade replaces the plugin set: compose disappears, buildx now
    // resolves through a different keg-contained symlink, and helper
    // becomes executable and is linked.
    let v2 = prepare(&store, "docker", "28.0", &[("docker", true)]);
    stage_file(&v2, "bin/buildx", b"#!/bin/sh\n", 0o755);
    stage_link(
        &v2,
        "lib/docker/cli-plugins/docker-buildx",
        "../../../bin/buildx",
    );
    stage_file(
        &v2,
        "lib/docker/cli-plugins/docker-helper",
        b"#!/bin/sh\n",
        0o755,
    );
    let v2_receipt = v2.receipt.clone();
    store
        .upgrade(vec![(v1_receipt, v2)])
        .expect("upgrade docker plugins");

    assert_eq!(
        fs::read(prefix.join("lib/docker/cli-plugins/docker-buildx")).unwrap(),
        b"#!/bin/sh\n"
    );
    assert!(fs::symlink_metadata(prefix.join("lib/docker/cli-plugins/docker-compose")).is_err());
    assert_eq!(
        fs::read(prefix.join("lib/docker/cli-plugins/docker-helper")).unwrap(),
        b"#!/bin/sh\n"
    );

    read_only(&store)
        .remove(std::slice::from_ref(&v2_receipt))
        .expect("remove docker plugins");
    assert!(fs::symlink_metadata(prefix.join("lib/docker/cli-plugins/docker-buildx")).is_err());
    assert!(fs::symlink_metadata(prefix.join("lib/docker/cli-plugins/docker-helper")).is_err());
}

#[cfg(unix)]
#[test]
fn upgrades_a_pre_documentation_layout_package_into_managed_links() {
    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();

    // An older brewrs prefix only has bin/sbin/opt; its receipt and layout
    // must remain readable and upgradable.
    let v1 = prepare(&store, "legacy", "1.0", &[("legacy", true)]);
    let v1_receipt = v1.receipt.clone();
    store.commit(vec![v1]).expect("install legacy layout");
    for directory in ["share", "etc", "lib"] {
        assert!(fs::symlink_metadata(prefix.join(directory)).is_err());
    }

    let v2 = prepare(&store, "legacy", "2.0", &[("legacy", true)]);
    stage_file(&v2, "share/man/man1/legacy.1", b"man", 0o644);
    stage_file(
        &v2,
        "share/fish/vendor_completions.d/legacy.fish",
        b"fish",
        0o644,
    );
    let v2_receipt = v2.receipt.clone();
    store
        .upgrade(vec![(v1_receipt, v2)])
        .expect("upgrade into managed links");
    assert_eq!(
        fs::read(prefix.join("share/man/man1/legacy.1")).unwrap(),
        b"man"
    );
    assert_eq!(
        fs::read(prefix.join("share/fish/vendor_completions.d/legacy.fish")).unwrap(),
        b"fish"
    );

    read_only(&store)
        .remove(std::slice::from_ref(&v2_receipt))
        .expect("remove upgraded package");
    assert!(fs::symlink_metadata(prefix.join("share/man/man1/legacy.1")).is_err());
    assert!(
        fs::symlink_metadata(prefix.join("share/fish/vendor_completions.d/legacy.fish")).is_err()
    );
}

#[cfg(unix)]
#[test]
fn refuses_managed_link_collisions_before_writing() {
    let (_temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();

    // A foreign bash completion blocks the whole commit.
    fs::create_dir_all(prefix.join("etc/bash_completion.d")).unwrap();
    fs::write(prefix.join("etc/bash_completion.d/tool"), b"foreign").unwrap();
    let before = tree(&prefix);
    let v = prepare(&store, "tool", "1.0", &[("tool", true)]);
    stage_file(&v, "etc/bash_completion.d/tool", b"own", 0o644);
    let error = store.commit(vec![v]).expect_err("foreign completion");
    assert!(error_chain(&error).contains("already exists"), "{error:#}");
    assert_eq!(tree(&prefix), before);
    assert!(store.installed().expect("installed").is_empty());

    // A foreign Docker plugin blocks the whole commit as well.
    fs::create_dir_all(prefix.join("lib/docker/cli-plugins")).unwrap();
    fs::write(
        prefix.join("lib/docker/cli-plugins/docker-buildx"),
        b"foreign",
    )
    .unwrap();
    let before = tree(&prefix);
    let docker = prepare(&store, "docker", "1.0", &[("docker", true)]);
    stage_file(
        &docker,
        "lib/docker/cli-plugins/docker-buildx",
        b"own",
        0o755,
    );
    let error = store.commit(vec![docker]).expect_err("foreign plugin");
    assert!(error_chain(&error).contains("already exists"), "{error:#}");
    assert_eq!(tree(&prefix), before);

    // A foreign zsh function blocks an upgrade and leaves the old version
    // installed and linked.
    let docs = prepare(&store, "docs", "1.0", &[("docs", true)]);
    stage_file(&docs, "share/man/man1/docs.1", b"v1", 0o644);
    let docs_receipt = docs.receipt.clone();
    store.commit(vec![docs]).expect("install docs");
    fs::create_dir_all(prefix.join("share/zsh/site-functions")).unwrap();
    fs::write(prefix.join("share/zsh/site-functions/_docs"), b"foreign").unwrap();
    let v2 = prepare(&store, "docs", "2.0", &[("docs", true)]);
    stage_file(&v2, "share/zsh/site-functions/_docs", b"v2", 0o644);
    let error = store
        .upgrade(vec![(docs_receipt, v2)])
        .expect_err("foreign zsh function");
    assert!(error_chain(&error).contains("already exists"), "{error:#}");
    let installed = store.installed().expect("installed");
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].version, "1.0");
    assert_eq!(
        fs::read(prefix.join("share/man/man1/docs.1")).unwrap(),
        b"v1"
    );
    assert_eq!(
        fs::read(prefix.join("share/zsh/site-functions/_docs")).unwrap(),
        b"foreign"
    );
}

#[cfg(unix)]
#[test]
fn failed_commit_rolls_back_new_link_directories_only() {
    let (_temp, store) = new_store();
    let prefix = store.prefix();
    fs::create_dir(prefix.join("share")).unwrap();
    let first = prepare(&store, "first", "1.0", &[]);
    stage_file(&first, "share/man/man1/first.1", b"man", 0o644);
    let second = prepare(&store, "second", "1.0", &[]);
    stage_file(&second, "share/man/man8/second.8", b"man", 0o644);
    let third = prepare(&store, "third", "1.0", &[]);
    let error = store
        .commit_inner(vec![first, second, third], Some(2))
        .unwrap_err();
    assert!(error.to_string().contains("injected commit failure"));
    assert!(store.installed().unwrap().is_empty());
    assert!(prefix.join("share").is_dir());
    assert!(!prefix.join("share/man").exists());
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_managed_link_ancestors() {
    let (temp, store) = new_store();
    let prefix = store.prefix().to_path_buf();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();

    // Install refuses to traverse a symlinked share/ ancestor.
    std::os::unix::fs::symlink(&outside, prefix.join("share")).unwrap();
    let docs = prepare(&store, "tool", "1.0", &[("tool", true)]);
    stage_file(&docs, "share/man/man1/tool.1", b"man", 0o644);
    let error = store.commit(vec![docs]).expect_err("symlinked share");
    assert!(error_chain(&error).contains("symlink"), "{error:#}");
    assert!(store.installed().expect("installed").is_empty());
    assert!(!outside.join("man").exists());
    fs::remove_file(prefix.join("share")).unwrap();

    // The same applies to lib/, the Docker plugin ancestor.
    std::os::unix::fs::symlink(&outside, prefix.join("lib")).unwrap();
    let docker = prepare(&store, "docker", "1.0", &[("docker", true)]);
    stage_file(
        &docker,
        "lib/docker/cli-plugins/docker-buildx",
        b"#!/bin/sh\n",
        0o755,
    );
    let error = store.commit(vec![docker]).expect_err("symlinked lib");
    assert!(error_chain(&error).contains("symlink"), "{error:#}");
    assert!(!outside.join("docker").exists());
    fs::remove_file(prefix.join("lib")).unwrap();

    // Removal fails closed instead of scanning through the link.
    let installed = install(&store, "tool", "1.0", &[("tool", true)]);
    std::os::unix::fs::symlink(&outside, prefix.join("share")).unwrap();
    let error = read_only(&store)
        .remove(std::slice::from_ref(&installed))
        .expect_err("removal through symlink");
    assert!(error_chain(&error).contains("symlink"), "{error:#}");
    assert_eq!(store.installed().expect("installed").len(), 1);
    assert_eq!(
        fs::read_link(prefix.join(BIN).join("tool")).unwrap(),
        Path::new("../Cellar/tool/1.0/bin/tool")
    );
    fs::remove_file(prefix.join("share")).unwrap();
    read_only(&store)
        .remove(std::slice::from_ref(&installed))
        .expect("remove after unlink");
}

#[test]
fn held_lock_is_reported_as_lock_held() {
    let (_temp, store) = new_store();
    let _held = InstallLock::acquire(store.prefix()).expect("first lock");
    let error = InstallLock::acquire(store.prefix())
        .err()
        .expect("second lock must fail");
    match error {
        StoreError::LockHeld { path, owner } => {
            assert!(path.ends_with(".brewrs/state/install.lock"), "{path:?}");
            assert!(owner.contains("pid="), "{owner}");
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }
}

#[test]
fn unsafe_receipt_name_is_reported_as_invalid_package_name() {
    let error = validate_receipt(&receipt("../evil", "1.0", true)).expect_err("unsafe name");
    assert!(
        matches!(&error, StoreError::InvalidPackageName { name } if name == "../evil"),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "refusing invalid package name '../evil'");
    let error = validate_receipt(&receipt("hello", "../1", true)).expect_err("unsafe version");
    assert!(
        matches!(&error, StoreError::InvalidVersion { name, version } if name == "hello" && version == "../1"),
        "{error:?}"
    );
}

#[test]
fn read_only_binding_is_reported_as_read_only() {
    let (_temp, store) = new_store();
    let error = read_only(&store).ensure_owned().expect_err("read-only");
    assert!(matches!(error, StoreError::ReadOnly { .. }), "{error:?}");
}

#[test]
fn io_failures_carry_the_action_and_path_as_a_source_chain() {
    let (temp, _store) = new_store();
    let missing = temp.path().join("prefix/.brewrs/state/receipts/none.json");
    let error = read_receipt(&missing).expect_err("missing receipt");
    assert!(
        matches!(&error, StoreError::Io { action: "failed to inspect", path, .. } if *path == missing),
        "{error:?}"
    );
    assert_eq!(
        error_chain(&error),
        format!(
            "failed to inspect '{}': No such file or directory (os error 2)",
            missing.display()
        )
    );
}
