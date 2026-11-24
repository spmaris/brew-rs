use super::*;
use crate::infra::cask_extract::StagedCaskLink;
use crate::infra::error::error_chain;
use sha2::{Digest, Sha256};
use std::os::unix::fs::symlink;
use tempfile::tempdir;

fn digest_a() -> String {
    "a".repeat(64)
}

fn digest_b() -> String {
    "b".repeat(64)
}

fn stage_binary_cask(
    store: &Store,
    token: &str,
    version: &str,
    sha: &str,
    bin_name: &str,
) -> PreparedCask {
    let stage = store.stage().unwrap();
    let caskroom = stage.path().join("caskroom");
    fs::create_dir_all(&caskroom).unwrap();
    fs::write(caskroom.join(bin_name), b"#!/bin/sh\necho bin\n").unwrap();
    let target = format!("../Caskroom/{token}/{version}/{bin_name}");
    PreparedCask {
        receipt: CaskReceipt {
            token: token.into(),
            version: version.into(),
            sha256: sha.into(),
            auto_updates: false,
            appdir: None,
            apps: Vec::new(),
            caskroom_files: vec![bin_name.into()],
            links: vec![InstalledCaskLink {
                dest: format!("bin/{bin_name}"),
                target: target.clone(),
            }],
            wrappers: Vec::new(),
            conflicts_with_casks: Vec::new(),
            conflicts_with_formulae: Vec::new(),
        },
        staged: StagedCaskPayload {
            app_bundles: Vec::new(),
            has_caskroom_files: true,
            caskroom_files: vec![bin_name.into()],
            symlinks: vec![StagedCaskLink {
                relative_dest: format!("bin/{bin_name}"),
                target,
            }],
            wrappers: Vec::new(),
        },
        stage,
    }
}

fn stage_app_and_wrapper_cask(
    store: &Store,
    appdir: &Path,
    token: &str,
    version: &str,
    bundle: &str,
    wrapper_name: &str,
) -> PreparedCask {
    let stage = store.stage().unwrap();
    let staged_app = stage.path().join("apps").join(bundle);
    fs::create_dir_all(staged_app.join("Contents/MacOS")).unwrap();
    fs::write(staged_app.join("Contents/Info.plist"), b"<plist/>").unwrap();
    fs::write(staged_app.join("Contents/MacOS/app"), b"bin").unwrap();

    let staged_wrapper = stage.path().join("wrappers").join(wrapper_name);
    fs::create_dir_all(staged_wrapper.parent().unwrap()).unwrap();
    let script = b"#!/bin/sh\nexec '/tmp/app' \"$@\"\n";
    fs::write(&staged_wrapper, script).unwrap();
    let wrapper_sha = hex::encode(Sha256::digest(script));

    PreparedCask {
        receipt: CaskReceipt {
            token: token.into(),
            version: version.into(),
            sha256: digest_a(),
            auto_updates: true,
            appdir: Some(appdir.to_path_buf()),
            apps: vec![InstalledApp {
                bundle_name: bundle.into(),
                path: appdir.join(bundle),
            }],
            caskroom_files: Vec::new(),
            links: Vec::new(),
            wrappers: vec![InstalledWrapper {
                dest: format!("bin/{wrapper_name}"),
                sha256: wrapper_sha.clone(),
            }],
            conflicts_with_casks: Vec::new(),
            conflicts_with_formulae: Vec::new(),
        },
        staged: StagedCaskPayload {
            app_bundles: vec![super::super::cask_extract::StagedAppBundle {
                bundle_name: bundle.into(),
                staged_dir: staged_app,
            }],
            has_caskroom_files: false,
            caskroom_files: Vec::new(),
            symlinks: Vec::new(),
            wrappers: vec![super::super::cask_extract::StagedWrapper {
                relative_dest: format!("bin/{wrapper_name}"),
                staged_file: staged_wrapper,
                sha256: wrapper_sha,
            }],
        },
        stage,
    }
}

#[test]
fn binary_cask_commit_list_upgrade_and_remove_lifecycle() {
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let store = Store::open(&prefix, true).unwrap();

    let v1 = stage_binary_cask(&store, "claude-code@latest", "1.0", &digest_a(), "claude");
    commit(&store, vec![v1]).unwrap();
    let list = installed(&store).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].version, "1.0");
    assert!(prefix.join("bin/claude").is_symlink());
    assert!(
        prefix
            .join("Caskroom/claude-code@latest/1.0/claude")
            .is_file()
    );

    let v2 = stage_binary_cask(&store, "claude-code@latest", "2.0", &digest_b(), "claude");
    upgrade(&store, vec![(list[0].clone(), v2)]).unwrap();
    let list = installed(&store).unwrap();
    assert_eq!(list[0].version, "2.0");
    assert!(!prefix.join("Caskroom/claude-code@latest/1.0").exists());
    assert!(
        prefix
            .join("Caskroom/claude-code@latest/2.0/claude")
            .is_file()
    );
    assert_eq!(
        fs::read_link(prefix.join("bin/claude")).unwrap(),
        Path::new("../Caskroom/claude-code@latest/2.0/claude")
    );

    remove(&store, &list).unwrap();
    assert!(installed(&store).unwrap().is_empty());
    assert!(!prefix.join("bin/claude").exists());
    assert!(!prefix.join("Caskroom/claude-code@latest").exists());
}

#[test]
fn custom_appdir_wrapper_validation_and_rollback_safety() {
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let custom_appdir = temp.path().join("MyApps");
    fs::create_dir(&custom_appdir).unwrap();
    let store = Store::open(&prefix, true).unwrap();

    assert_eq!(
        resolve_appdir(&prefix, Some(&custom_appdir)).unwrap(),
        custom_appdir
    );
    assert!(resolve_appdir(&prefix, Some(Path::new("/opt/homebrew"))).is_err());
    let link_appdir = temp.path().join("link-apps");
    symlink(&custom_appdir, &link_appdir).unwrap();
    assert!(resolve_appdir(&prefix, Some(&link_appdir)).is_err());

    let app_cask = stage_app_and_wrapper_cask(
        &store,
        &custom_appdir,
        "demo-app",
        "1.0",
        "Demo.app",
        "demo",
    );
    commit(&store, vec![app_cask]).unwrap();
    assert!(!caskroom_dir(&prefix).join("demo-app").exists());
    let list = installed(&store).unwrap();
    assert!(custom_appdir.join("Demo.app/Contents/Info.plist").is_file());
    assert!(prefix.join("bin/demo").is_file());

    // Edited wrapper must block removal.
    let original = fs::read(prefix.join("bin/demo")).unwrap();
    fs::write(prefix.join("bin/demo"), b"tampered").unwrap();
    assert!(remove(&store, &list).is_err());
    assert!(casks_state_dir(&prefix).join("demo-app.json").is_file());

    // Restore original bytes, removal succeeds, and `.app`-only cask can be reinstalled.
    fs::write(prefix.join("bin/demo"), original).unwrap();
    remove(&store, &list).unwrap();
    assert!(!custom_appdir.join("Demo.app").exists());
    assert!(!prefix.join("bin/demo").exists());
    let reinstall = stage_app_and_wrapper_cask(
        &store,
        &custom_appdir,
        "demo-app",
        "1.0",
        "Demo.app",
        "demo",
    );
    commit(&store, vec![reinstall]).unwrap();
    let list = installed(&store).unwrap();
    remove(&store, &list).unwrap();

    // Foreign app collision prevents commit and rolls back earlier items in batch.
    fs::create_dir_all(custom_appdir.join("Keep.app")).unwrap();
    let first = stage_binary_cask(&store, "first", "1.0", &digest_a(), "first");
    let colliding = stage_app_and_wrapper_cask(
        &store,
        &custom_appdir,
        "second",
        "1.0",
        "Keep.app",
        "second",
    );
    assert!(commit(&store, vec![first, colliding]).is_err());
    assert!(installed(&store).unwrap().is_empty());
    assert!(custom_appdir.join("Keep.app").is_dir());
}

#[test]
fn enforces_cask_and_formula_conflicts_and_symlink_checks() {
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let store = Store::open(&prefix, true).unwrap();

    let mut c1 = stage_binary_cask(&store, "claude-code", "1.0", &digest_a(), "claude");
    c1.receipt.conflicts_with_casks = vec!["claude-code@latest".into()];
    commit(&store, vec![c1]).unwrap();

    let c2 = stage_binary_cask(&store, "claude-code@latest", "2.0", &digest_b(), "claude2");
    let err = commit(&store, vec![c2]).unwrap_err();
    assert!(error_chain(&err).contains("conflicts with"), "{err:#}");

    let list = installed(&store).unwrap();
    let mut formula_conflict = list[0].clone();
    formula_conflict.conflicts_with_formulae = vec!["node".into()];
    let f_receipt = Receipt {
        source: Default::default(),
        name: "node".into(),
        version: "1.0".into(),
        version_scheme: 0,
        sha256: digest_a(),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: Vec::new(),
    };
    assert!(validate_cask_conflicts(&[formula_conflict], &[f_receipt]).is_err());

    // Repointed symlink blocks removal.
    fs::remove_file(prefix.join("bin/claude")).unwrap();
    symlink("../Caskroom/other/1.0/claude", prefix.join("bin/claude")).unwrap();
    assert!(remove(&store, &list).is_err());
}

#[test]
fn cask_conflict_is_reported_as_a_typed_variant() {
    let mut cask = CaskReceipt {
        token: "alpha".into(),
        version: "1.0".into(),
        sha256: digest_a(),
        auto_updates: false,
        appdir: None,
        apps: Vec::new(),
        caskroom_files: Vec::new(),
        links: Vec::new(),
        wrappers: Vec::new(),
        conflicts_with_casks: vec!["beta".into()],
        conflicts_with_formulae: Vec::new(),
    };
    let mut beta = cask.clone();
    beta.token = "beta".into();
    beta.conflicts_with_casks = Vec::new();
    let error = validate_cask_conflicts(&[cask.clone(), beta], &[]).unwrap_err();
    assert!(
        matches!(&error, CaskStoreError::CaskConflict { token, conflict } if token == "alpha" && conflict == "beta"),
        "{error:?}"
    );
    cask.token = "Bad Token".into();
    let error = validate_cask_receipt(&cask).unwrap_err();
    assert!(
        matches!(error, CaskStoreError::InvalidIdentifier(_)),
        "{error:?}"
    );
}
