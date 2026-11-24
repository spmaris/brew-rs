use super::*;
use crate::config::Config;
use std::os::unix::fs::PermissionsExt;

const TAP: &str = "owner/tap";

fn config() -> Config {
    let mut config = Config::default();
    config.ui.colored = false;
    config
}

fn new_prefix() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let prefix = temp.path().join("prefix");
    Store::open(&prefix, true).expect("initialize prefix");
    (temp, prefix)
}

fn manifest(
    tap: &str,
    token: &str,
    bin: &str,
    version: &str,
    architecture: &str,
    url: &str,
    sha256: &str,
) -> String {
    format!(
        r#"schema = 1
tap = "{tap}"

[[packages]]
token = "{token}"
version = "{version}"
bin = "{bin}"

[packages.artifacts.{architecture}]
url = "{url}"
sha256 = "{sha256}"
"#
    )
}

fn fixture_macho() -> Option<(&'static str, Vec<u8>)> {
    let architecture = host_arch().ok()?;
    let cpu_type = match architecture {
        "arm64" => 0x0100_000c_u32,
        _ => 0x0100_0007_u32,
    };
    let mut bytes = vec![0_u8; 64];
    bytes[0..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&cpu_type.to_le_bytes());
    bytes[12..16].copy_from_slice(&0x2_u32.to_le_bytes());
    Some((architecture, bytes))
}

fn setup_tap(
    temp: &Path,
    prefix: &Path,
    token: &str,
    version: &str,
    artifact: &[u8],
    architecture: &str,
) -> PathBuf {
    let sha256 = sha256_hex(artifact);
    let url =
        format!("https://github.com/owner/tap/releases/download/v{version}/{token}-{architecture}");
    let path = temp.join(format!("{token}-tap.toml"));
    fs::write(
        &path,
        manifest(TAP, token, token, version, architecture, &url, &sha256),
    )
    .unwrap();
    add(&config(), Some(prefix), TAP, &path).unwrap();
    path
}

fn install_fixture(temp: &Path, prefix: &Path) -> Option<(TapPlan, TapReceipt)> {
    let (architecture, artifact) = fixture_macho()?;
    setup_tap(temp, prefix, "tool", "1.0", &artifact, architecture);
    let plan = build_plan(prefix, "owner/tap/tool").unwrap();

    let source = temp.join("downloaded");
    fs::write(&source, &artifact).unwrap();
    let stage = tempfile::Builder::new()
        .prefix("tap-test-")
        .tempdir_in(stages_dir(prefix))
        .unwrap();
    let staged = stage.path().join("tool");
    let bytes = stage_verified(&source, &staged, &plan.artifact_sha256, plan.architecture).unwrap();
    let receipt = TapReceipt::new(
        &plan.tap,
        &plan.token,
        &plan.version,
        &plan.bin,
        plan.architecture,
        &plan.artifact_url,
        &plan.artifact_sha256,
        bytes,
        &plan.manifest_sha256,
        &plan.source_manifest,
    )
    .unwrap();

    let store = Store::open_existing(prefix).unwrap();
    let lock = InstallLock::acquire(prefix).unwrap();
    commit_install(&store, &plan, &staged, &encode_json(&receipt).unwrap()).unwrap();
    drop(lock);
    Some((plan, receipt))
}

#[test]
fn removal_refuses_symlinked_taproom_ancestors_and_preserves_external_files() {
    let (temp, prefix) = new_prefix();
    let Some((_plan, receipt)) = install_fixture(temp.path(), &prefix) else {
        return;
    };
    let executable_rel = Path::new("Taproom/owner/tap/tool/1.0/bin/tool");

    // Replace the real Taproom with a symlink to an external copy of the
    // installed payload. Verification must refuse before any rename.
    let taproom = prefix.join("Taproom");
    let external_taproom = temp.path().join("external-taproom");
    fs::rename(&taproom, &external_taproom).unwrap();
    fs::write(external_taproom.join("sentinel"), b"keep me").unwrap();
    std::os::unix::fs::symlink(&external_taproom, &taproom).unwrap();

    let error = verify_installed_artifact(&prefix, &receipt).unwrap_err();
    assert!(format!("{error:#}").contains("symlink"));
    let error = commit_uninstall(&prefix, &receipt).unwrap_err();
    assert!(format!("{error:#}").contains("symlink"));
    assert_eq!(
        fs::read(external_taproom.join("owner/tap/tool/1.0/bin/tool")).unwrap(),
        fs::read(prefix.join(executable_rel)).unwrap()
    );
    assert_eq!(
        fs::read(external_taproom.join("sentinel")).unwrap(),
        b"keep me"
    );
    assert!(prefix.join("bin/tool").exists());

    fs::remove_file(&taproom).unwrap();
    fs::rename(&external_taproom, &taproom).unwrap();

    // The owner component must be guarded just like Taproom itself.
    let owner = prefix.join("Taproom/owner");
    let external_owner = temp.path().join("external-owner");
    fs::rename(&owner, &external_owner).unwrap();
    fs::write(external_owner.join("sentinel"), b"keep me too").unwrap();
    std::os::unix::fs::symlink(&external_owner, &owner).unwrap();

    let error = verify_installed_artifact(&prefix, &receipt).unwrap_err();
    assert!(format!("{error:#}").contains("symlink"));
    let error = commit_uninstall(&prefix, &receipt).unwrap_err();
    assert!(format!("{error:#}").contains("symlink"));
    assert_eq!(
        fs::read(external_owner.join("tap/tool/1.0/bin/tool")).unwrap(),
        fs::read(prefix.join(executable_rel)).unwrap()
    );
    assert_eq!(
        fs::read(external_owner.join("sentinel")).unwrap(),
        b"keep me too"
    );
    assert!(prefix.join("bin/tool").exists());

    fs::remove_file(&owner).unwrap();
    fs::rename(&external_owner, &owner).unwrap();

    let lock = InstallLock::acquire(&prefix).unwrap();
    commit_uninstall(&prefix, &receipt).unwrap();
    drop(lock);
    assert!(!prefix.join("bin/tool").exists());
    assert!(!prefix.join(executable_rel).exists());
}

#[test]
fn add_list_plan_and_untap_are_bounded_and_remove_no_packages() {
    let (temp, prefix) = new_prefix();
    let Some((architecture, artifact)) = fixture_macho() else {
        return;
    };
    let manifest_path = setup_tap(temp.path(), &prefix, "tool", "1.0", &artifact, architecture);

    let dir = prefix.join(".brewrs/state/taps/owner/tap");
    assert!(dir.join("tap.json").is_file());
    assert!(dir.join("manifest.toml").is_file());
    assert_eq!(
        fs::read(dir.join("manifest.toml")).unwrap(),
        fs::read(&manifest_path).unwrap()
    );

    let plan = build_plan(&prefix, "owner/tap/tool").unwrap();
    assert_eq!(plan.qualified, "owner/tap/tool");
    assert_eq!(plan.version, "1.0");
    assert_eq!(plan.architecture, architecture);
    assert_eq!(plan.artifact_sha256, sha256_hex(&artifact));
    assert!(plan.installed.is_none());
    assert_eq!(plan.keg, "Taproom/owner/tap/tool/1.0");
    assert_eq!(plan.link, "bin/tool");

    let taps = all_tapped(&prefix).unwrap();
    assert_eq!(taps.len(), 1);
    assert_eq!(taps[0].manifest.package("tool").unwrap().version, "1.0");
    assert!(taps[0].manifest.package("missing").is_err());

    assert!(add(&config(), Some(&prefix), TAP, &manifest_path).is_err());
    untap(&config(), Some(&prefix), TAP).unwrap();
    assert!(!dir.exists());
    assert!(!prefix.join("bin/tool").exists());
}

#[test]
fn install_writes_exact_receipt_link_and_payload_then_uninstalls() {
    let (temp, prefix) = new_prefix();
    let Some((architecture, artifact)) = fixture_macho() else {
        return;
    };
    setup_tap(temp.path(), &prefix, "tool", "1.0", &artifact, architecture);
    let plan = build_plan(&prefix, "owner/tap/tool").unwrap();

    let source = temp.path().join("downloaded");
    fs::write(&source, &artifact).unwrap();
    let stage = tempfile::Builder::new()
        .prefix("tap-test-")
        .tempdir_in(stages_dir(&prefix))
        .unwrap();
    let staged = stage.path().join("tool");
    let bytes = stage_verified(&source, &staged, &plan.artifact_sha256, plan.architecture).unwrap();
    let receipt = TapReceipt::new(
        &plan.tap,
        &plan.token,
        &plan.version,
        &plan.bin,
        plan.architecture,
        &plan.artifact_url,
        &plan.artifact_sha256,
        bytes,
        &plan.manifest_sha256,
        &plan.source_manifest,
    )
    .unwrap();

    let store = Store::open_existing(&prefix).unwrap();
    let lock = InstallLock::acquire(&prefix).unwrap();
    commit_install(&store, &plan, &staged, &encode_json(&receipt).unwrap()).unwrap();
    drop(lock);

    let link = prefix.join("bin/tool");
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("../Taproom/owner/tap/tool/1.0/bin/tool")
    );
    let executable = prefix.join("Taproom/owner/tap/tool/1.0/bin/tool");
    assert_eq!(fs::read(&executable).unwrap(), artifact);
    assert_eq!(
        fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let receipt_path = prefix.join(".brewrs/state/taps/owner/tap/receipts/tool.json");
    let stored: TapReceipt = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(stored, receipt);

    let installed = build_plan(&prefix, "owner/tap/tool").unwrap();
    assert_eq!(installed.installed.as_ref(), Some(&receipt));
    assert_eq!(verify_installed_artifact(&prefix, &receipt).unwrap(), bytes);

    assert!(untap(&config(), Some(&prefix), TAP).is_err());

    let lock = InstallLock::acquire(&prefix).unwrap();
    commit_uninstall(&prefix, &receipt).unwrap();
    drop(lock);

    assert!(!link.exists());
    assert!(!executable.exists());
    assert!(!receipt_path.exists());
    assert!(!prefix.join("Taproom/owner/tap/tool").exists());

    untap(&config(), Some(&prefix), TAP).unwrap();
    assert!(!prefix.join(".brewrs/state/taps/owner/tap").exists());
    assert!(!prefix.join("Taproom").exists());
}

#[test]
fn a_failed_publish_never_removes_a_preexisting_receipt() {
    let (temp, prefix) = new_prefix();
    let Some((architecture, artifact)) = fixture_macho() else {
        return;
    };
    setup_tap(temp.path(), &prefix, "tool", "1.0", &artifact, architecture);
    let plan = build_plan(&prefix, "owner/tap/tool").unwrap();

    let source = temp.path().join("downloaded");
    fs::write(&source, &artifact).unwrap();
    let stage = tempfile::Builder::new()
        .prefix("tap-test-")
        .tempdir_in(stages_dir(&prefix))
        .unwrap();
    let staged = stage.path().join("tool");
    let bytes = stage_verified(&source, &staged, &plan.artifact_sha256, plan.architecture).unwrap();
    let receipt = TapReceipt::new(
        &plan.tap,
        &plan.token,
        &plan.version,
        &plan.bin,
        plan.architecture,
        &plan.artifact_url,
        &plan.artifact_sha256,
        bytes,
        &plan.manifest_sha256,
        &plan.source_manifest,
    )
    .unwrap();

    let receipt_dir = plan.receipt_path.parent().unwrap();
    fs::create_dir_all(receipt_dir).unwrap();
    fs::write(&plan.receipt_path, b"pre-existing receipt").unwrap();

    let store = Store::open_existing(&prefix).unwrap();
    let error =
        commit_install(&store, &plan, &staged, &encode_json(&receipt).unwrap()).unwrap_err();
    assert!(format!("{error:#}").contains("refusing to overwrite"));
    assert_eq!(
        fs::read(&plan.receipt_path).unwrap(),
        b"pre-existing receipt"
    );
    assert!(!prefix.join(&plan.link).exists());
    assert!(!prefix.join(&plan.keg).exists());
}

#[test]
fn install_refuses_existing_links_and_official_name_shadowing() {
    let (temp, prefix) = new_prefix();
    let Some((architecture, artifact)) = fixture_macho() else {
        return;
    };
    setup_tap(temp.path(), &prefix, "tool", "1.0", &artifact, architecture);

    fs::write(prefix.join("bin/tool"), b"foreign").unwrap();
    let error = build_plan(&prefix, "owner/tap/tool").unwrap_err();
    assert!(format!("{error:#}").contains("already exists"));
    fs::remove_file(prefix.join("bin/tool")).unwrap();

    let receipt = serde_json::json!({
        "name": "tool",
        "version": "1.0",
        "version_scheme": 0,
        "sha256": "a".repeat(64),
        "requested": true,
        "keg_only": false,
        "conflicts_with": [],
        "dependencies": []
    });
    fs::write(
        prefix.join(".brewrs/state/receipts/tool.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    let error = build_plan(&prefix, "owner/tap/tool").unwrap_err();
    assert!(format!("{error:#}").contains("standard brewrs formula"));
}

#[test]
fn add_rejects_mismatched_manifest_taps_and_bad_paths() {
    let (temp, prefix) = new_prefix();
    let Some((architecture, artifact)) = fixture_macho() else {
        return;
    };
    let sha256 = sha256_hex(&artifact);
    let mismatched = manifest(
        "other/tap",
        "tool",
        "tool",
        "1.0",
        architecture,
        "https://example.com/tool",
        &sha256,
    );
    let path = temp.path().join("mismatched.toml");
    fs::write(&path, mismatched).unwrap();
    assert!(add(&config(), Some(&prefix), TAP, &path).is_err());

    let symlink = temp.path().join("link.toml");
    std::os::unix::fs::symlink(&path, &symlink).unwrap();
    assert!(add(&config(), Some(&prefix), TAP, &symlink).is_err());

    assert!(add(&config(), Some(&prefix), "homebrew/core", &path).is_err());
    assert!(
        build_plan(&prefix, "owner/tap/tool")
            .unwrap_err()
            .to_string()
            .contains("not registered")
    );
}
