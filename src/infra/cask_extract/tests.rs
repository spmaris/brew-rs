use std::fs::{self, File};
use std::path::PathBuf;
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use super::dmg_extract::parse_hdiutil_mount_point;
use super::format::detect_format;
use super::model::PayloadFormat;
use super::wrapper::set_executable;
use super::*;
use crate::domain::cask_plan::{CaskSourcePath, ContainerHint, PlannedCaskArtifact};

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn detects_container_formats_and_stages_naked_binary() {
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let appdir = prefix.join("Applications");

    let binary_bytes = b"#!/bin/sh\necho hi\n";
    let digest = sha256_hex(binary_bytes);
    let download = temp.path().join("claude.download");
    fs::write(&download, binary_bytes).unwrap();
    assert_eq!(
        detect_format(&download, "https://example.com/claude", ContainerHint::Auto).unwrap(),
        PayloadFormat::Naked
    );

    let macho = temp.path().join("macho.download");
    fs::write(&macho, [0xCF, 0xFA, 0xED, 0xFE, 0, 0, 0, 0]).unwrap();
    assert_eq!(
        detect_format(&macho, "https://example.com/bin", ContainerHint::Auto).unwrap(),
        PayloadFormat::Naked
    );

    let stage_dir = temp.path().join("stage");
    fs::create_dir(&stage_dir).unwrap();
    let planned = PlannedCask {
        token: "claude-code@latest".into(),
        name: vec!["Claude Code".into()],
        version: "2.1.330".into(),
        auto_updates: false,
        url: "https://example.com/claude".into(),
        sha256: digest,
        container: ContainerHint::Auto,
        artifacts: vec![PlannedCaskArtifact::Binary {
            source: CaskSourcePath::Payload("claude".into()),
            link_name: "claude".into(),
        }],
        conflicts_with_casks: Vec::new(),
        conflicts_with_formulae: Vec::new(),
        install_blockers: Vec::new(),
    };

    let staged = stage(&download, &stage_dir, &planned, &prefix, &appdir).unwrap();
    assert!(staged.has_caskroom_files);
    assert_eq!(staged.caskroom_files, vec!["claude"]);
    assert!(stage_dir.join("caskroom/claude").is_file());
    assert_eq!(staged.symlinks.len(), 1);
    assert_eq!(staged.symlinks[0].relative_dest, "bin/claude");
    assert_eq!(
        staged.symlinks[0].target,
        "../Caskroom/claude-code@latest/2.1.330/claude"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn preflights_zip_with_ditto_preserving_bundle_symlinks_and_builds_wrapper() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().unwrap();
    let src_root = temp.path().join("zip-src");
    let app = src_root.join("Demo App.app/Contents");
    fs::create_dir_all(app.join("MacOS")).unwrap();
    fs::create_dir_all(app.join("Frameworks/Sub.framework/Versions/A")).unwrap();
    fs::write(app.join("Info.plist"), b"<plist/>").unwrap();
    fs::write(app.join("MacOS/demo"), b"#!/bin/sh\necho ok\n").unwrap();
    set_executable(&app.join("MacOS/demo")).unwrap();
    fs::write(app.join("Frameworks/Sub.framework/Versions/A/Sub"), b"lib").unwrap();
    symlink("A", app.join("Frameworks/Sub.framework/Versions/Current")).unwrap();
    symlink(
        "Versions/Current/Sub",
        app.join("Frameworks/Sub.framework/Sub"),
    )
    .unwrap();

    let zip_path = temp.path().join("demo.zip");
    let status = Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(src_root.join("Demo App.app"))
        .arg(&zip_path)
        .status()
        .unwrap();
    assert!(status.success());

    let zip_bytes = fs::read(&zip_path).unwrap();
    let digest = sha256_hex(&zip_bytes);
    let prefix = temp.path().join("prefix");
    let appdir = prefix.join("Applications");
    let stage_dir = temp.path().join("stage");
    fs::create_dir(&stage_dir).unwrap();

    let planned = PlannedCask {
        token: "demo".into(),
        name: vec!["Demo App".into()],
        version: "1.0".into(),
        auto_updates: true,
        url: "https://example.com/demo.zip".into(),
        sha256: digest,
        container: ContainerHint::Zip,
        artifacts: vec![
            PlannedCaskArtifact::App {
                source: "Demo App.app".into(),
                bundle_name: "Demo App.app".into(),
            },
            PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::AppBundle {
                    bundle_name: "Demo App.app".into(),
                    inner_path: "Contents/MacOS/demo".into(),
                },
                link_name: "demo-cli".into(),
                args: vec!["--flag=a'b".into()],
            },
        ],
        conflicts_with_casks: Vec::new(),
        conflicts_with_formulae: Vec::new(),
        install_blockers: Vec::new(),
    };

    let staged = stage(&zip_path, &stage_dir, &planned, &prefix, &appdir).unwrap();
    assert!(!staged.has_caskroom_files);
    assert_eq!(staged.app_bundles.len(), 1);
    assert!(
        staged.app_bundles[0]
            .staged_dir
            .join("Contents/Frameworks/Sub.framework/Sub")
            .is_symlink()
    );
    assert_eq!(staged.wrappers.len(), 1);
    let wrapper_contents = fs::read_to_string(&staged.wrappers[0].staged_file).unwrap();
    assert!(wrapper_contents.starts_with("#!/bin/sh\nexec '"));
    assert!(wrapper_contents.contains("'--flag=a'\\''b' \"$@\"\n"));
    assert_eq!(
        staged.wrappers[0].sha256,
        sha256_hex(wrapper_contents.as_bytes())
    );
}

#[test]
fn rejects_zip_slip_central_directory_and_escaping_symlinks() {
    // Minimal hand-crafted ZIP with `../evil` in Central Directory.
    let fname = b"../evil";
    let mut cd = Vec::new();
    cd.extend_from_slice(b"PK\x01\x02");
    cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    cd.extend_from_slice(&0_u32.to_le_bytes()); // crc
    cd.extend_from_slice(&0_u32.to_le_bytes()); // comp
    cd.extend_from_slice(&4_u32.to_le_bytes()); // uncomp
    cd.extend_from_slice(&(fname.len() as u16).to_le_bytes());
    cd.extend_from_slice(&0_u16.to_le_bytes());
    cd.extend_from_slice(&0_u16.to_le_bytes());
    cd.extend_from_slice(&[0; 8]);
    cd.extend_from_slice(&0_u32.to_le_bytes());
    cd.extend_from_slice(fname);

    let mut zip = Vec::new();
    let cd_offset = zip.len() as u32;
    let cd_size = cd.len() as u32;
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(b"PK\x05\x06");
    zip.extend_from_slice(&[0; 4]);
    zip.extend_from_slice(&1_u16.to_le_bytes());
    zip.extend_from_slice(&1_u16.to_le_bytes());
    zip.extend_from_slice(&cd_size.to_le_bytes());
    zip.extend_from_slice(&cd_offset.to_le_bytes());
    zip.extend_from_slice(&0_u16.to_le_bytes());

    let temp = tempdir().unwrap();
    let zip_path = temp.path().join("slip.zip");
    fs::write(&zip_path, &zip).unwrap();
    let err = preflight_zip(&zip_path).unwrap_err();
    assert!(format!("{err:#}").contains("disallowed path"), "{err:#}");

    let tree = temp.path().join("tree");
    fs::create_dir(&tree).unwrap();
    std::os::unix::fs::symlink("../../etc/passwd", tree.join("escape")).unwrap();
    let err = validate_extracted_tree(&tree).unwrap_err();
    assert!(format!("{err:#}").contains("escaping symlink"), "{err:#}");
}

#[test]
fn stages_tar_gz_cask_payload() {
    let temp = tempdir().unwrap();
    let tgz_path = temp.path().join("tool.tar.gz");
    {
        let file = File::create(&tgz_path).unwrap();
        let enc = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(enc);
        let bin_data = b"#!/bin/sh\necho tool\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(bin_data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "bin/mytool", &bin_data[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let digest = sha256_hex(&fs::read(&tgz_path).unwrap());
    let prefix = temp.path().join("prefix");
    let appdir = prefix.join("Applications");
    let stage_dir = temp.path().join("stage");
    fs::create_dir(&stage_dir).unwrap();

    let planned = PlannedCask {
        token: "mytool".into(),
        name: vec![],
        version: "1.2".into(),
        auto_updates: false,
        url: "https://example.com/tool.tar.gz".into(),
        sha256: digest,
        container: ContainerHint::Auto,
        artifacts: vec![PlannedCaskArtifact::Binary {
            source: CaskSourcePath::Payload("bin/mytool".into()),
            link_name: "mytool".into(),
        }],
        conflicts_with_casks: Vec::new(),
        conflicts_with_formulae: Vec::new(),
        install_blockers: Vec::new(),
    };

    let staged = stage(&tgz_path, &stage_dir, &planned, &prefix, &appdir).unwrap();
    assert_eq!(staged.caskroom_files, vec!["bin/mytool"]);
    assert_eq!(
        staged.symlinks[0].target,
        "../Caskroom/mytool/1.2/bin/mytool"
    );
}

#[test]
fn parses_hdiutil_plist_mount_points_with_spaces_and_entities() {
    let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>system-entities</key>
  <array>
    <dict>
      <key>content-hint</key>
      <string>GUID_partition_scheme</string>
    </dict>
    <dict>
      <key>content-hint</key>
      <string>Apple_HFS</string>
      <key>mount-point</key>
      <string>/tmp/smoke/dmg.abc123/App &amp; Tools</string>
    </dict>
  </array>
</dict>
</plist>"#;
    let path = parse_hdiutil_mount_point(plist).unwrap();
    assert_eq!(path, PathBuf::from("/tmp/smoke/dmg.abc123/App & Tools"));
    assert!(parse_hdiutil_mount_point("<plist><dict></dict></plist>").is_err());
}

#[test]
fn bare_io_errors_gain_an_action_and_path() {
    let error = CaskExtractError::io("create directory", Path::new("/unpack/Foo.app"))(
        std::io::Error::other("denied"),
    );
    assert_eq!(
        crate::infra::error::error_chain(&error),
        "failed to create directory '/unpack/Foo.app': denied"
    );
}
