use super::apply::{compare_rewrite, signing_arguments, verify_stage};
use super::constants::*;
use super::fat::{FAT_CIGAM_BYTES, FAT_MAGIC_64_BYTES, FAT_MAGIC_BYTES};
use super::model::{Kind, Parsed};
use super::parse::{MACH_HEADER_64_BYTES, MAX_LOAD_COMMANDS, validate_name};
use super::rewrite_plan::{Change, Plan, scan_markers};
use super::*;
use crate::infra::relocate::RelocateError;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const SYSTEM_LIB: &str = "/usr/lib/libSystem.B.dylib";
const PREFIX: &str = "/Users/example/Library/Application Support/brew-rs";
const CELLAR: &str = "@@HOMEBREW_CELLAR@@/pcre2/10.49/lib/libpcre2-8.0.dylib";
const PREFIXED: &str = "@@HOMEBREW_PREFIX@@/opt/pcre2/lib/libpcre2-8.0.dylib";

fn align8(value: usize) -> usize {
    (value + 7) & !7
}

fn dylib_command(command: u32, name: &str) -> Vec<u8> {
    raw_dylib_command(command, 24, name.as_bytes(), true)
}

/// Build a dylib command with an explicit string offset and raw tail bytes.
fn raw_dylib_command(command: u32, string_offset: u32, tail: &[u8], terminate: bool) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&command.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // patched below
    body.extend_from_slice(&string_offset.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // timestamp
    body.extend_from_slice(&0u32.to_le_bytes()); // current version
    body.extend_from_slice(&0u32.to_le_bytes()); // compatibility version
    body.extend_from_slice(tail);
    if terminate {
        body.push(0);
    }
    let size = align8(body.len());
    body[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    body.resize(size, 0);
    body
}

fn rpath_command(path: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&LC_RPATH.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // patched below
    body.extend_from_slice(&12u32.to_le_bytes());
    body.extend_from_slice(path.as_bytes());
    body.push(0);
    let size = align8(body.len());
    body[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    body.resize(size, 0);
    body
}

fn opaque_command(command: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&command.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // patched below
    body.extend_from_slice(payload);
    let size = align8(body.len());
    body[4..8].copy_from_slice(&(size as u32).to_le_bytes());
    body.resize(size, 0);
    body
}

fn code_signature_command(data_offset: u32, data_size: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&LC_CODE_SIGNATURE.to_le_bytes());
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&data_offset.to_le_bytes());
    body.extend_from_slice(&data_size.to_le_bytes());
    body
}

struct Image {
    cputype: u32,
    cpusubtype: u32,
    filetype: u32,
    commands: Vec<Vec<u8>>,
    trailer: Vec<u8>,
}

impl Image {
    fn new() -> Self {
        Self {
            cputype: CPU_TYPE_ARM64,
            cpusubtype: 0,
            filetype: MH_EXECUTE,
            commands: Vec::new(),
            trailer: Vec::new(),
        }
    }

    fn dylib(mut self) -> Self {
        self.filetype = MH_DYLIB;
        self
    }

    fn command(mut self, command: Vec<u8>) -> Self {
        self.commands.push(command);
        self
    }

    fn trailer(mut self, trailer: &[u8]) -> Self {
        self.trailer.extend_from_slice(trailer);
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let command_bytes: usize = self.commands.iter().map(Vec::len).sum();
        let mut bytes =
            Vec::with_capacity(MACH_HEADER_64_BYTES + command_bytes + self.trailer.len());
        bytes.extend_from_slice(&MH_MAGIC_64_BYTES);
        bytes.extend_from_slice(&self.cputype.to_le_bytes());
        bytes.extend_from_slice(&self.cpusubtype.to_le_bytes());
        bytes.extend_from_slice(&self.filetype.to_le_bytes());
        bytes.extend_from_slice(&(self.commands.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(command_bytes as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
        for command in &self.commands {
            bytes.extend_from_slice(command);
        }
        bytes.extend_from_slice(&self.trailer);
        bytes
    }
}

/// Append a signature command and a dummy signature body.
fn with_signature(mut image: Image) -> Vec<u8> {
    let before_signature: usize = image.commands.iter().map(Vec::len).sum();
    let data_offset = MACH_HEADER_64_BYTES + before_signature + 16;
    image
        .commands
        .push(code_signature_command(data_offset as u32, 16));
    let mut bytes = image.bytes();
    bytes.extend_from_slice(&[0u8; 16]);
    bytes
}

fn parse_ok(image: Image) -> Parsed {
    let bytes = image.bytes();
    parse(&bytes).expect("synthetic image should parse")
}

fn plan_for(
    parsed: &Parsed,
    mapper: impl FnMut(PathKind, &str, &[&str]) -> Result<Option<String>, RelocateError>,
) -> Result<Plan, MachoError> {
    let mut mapper = mapper;
    plan(parsed, &mut mapper)
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(bytes: &[u8]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "brew-rs-macho-relocate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, bytes).expect("temporary image should be writable");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn placeholder_mapper(
    _kind: PathKind,
    path: &str,
    _rpaths: &[&str],
) -> Result<Option<String>, RelocateError> {
    if path.starts_with("@@HOMEBREW_PREFIX@@/") {
        Ok(Some(path.replacen("@@HOMEBREW_PREFIX@@", PREFIX, 1)))
    } else if path.starts_with("@@HOMEBREW_CELLAR@@/") {
        Ok(Some(path.replacen(
            "@@HOMEBREW_CELLAR@@",
            &format!("{PREFIX}/Cellar"),
            1,
        )))
    } else {
        Ok(None)
    }
}

#[test]
fn recognizes_thin_images_and_rejects_java_classes() {
    let thin64 = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();
    assert!(is_macho(&thin64));

    let mut thin32 = thin64.clone();
    thin32[..4].copy_from_slice(&MH_MAGIC_32_BYTES);
    assert!(is_macho(&thin32));

    let mut big64 = thin64.clone();
    big64[..4].copy_from_slice(&MH_CIGAM_64_BYTES);
    assert!(is_macho(&big64));

    assert!(!is_macho(b""));
    assert!(!is_macho(b"\xcf\xfa\xed"));
    assert!(!is_macho(b"not a mach-o file"));

    // A Java class file begins with 0xcafebabe but its version field is
    // where nfat_arch would be; 52 slices is not plausible.
    let mut java = Vec::new();
    java.extend_from_slice(&FAT_MAGIC_BYTES);
    java.extend_from_slice(&0u16.to_be_bytes()); // minor
    java.extend_from_slice(&52u16.to_be_bytes()); // major
    java.resize(4096, 0);
    assert!(!is_macho(&java));

    let fat = plausible_fat();
    assert!(is_macho(&fat));
    assert!(!is_macho(&fat[..8]));
}

fn plausible_fat() -> Vec<u8> {
    let offset: u32 = 0x1000;
    let size: u32 = 8;
    let mut bytes = vec![0u8; (offset + size) as usize];
    bytes[..4].copy_from_slice(&FAT_MAGIC_BYTES);
    bytes[4..8].copy_from_slice(&1u32.to_be_bytes());
    bytes[8..12].copy_from_slice(&CPU_TYPE_ARM64.to_be_bytes());
    bytes[12..16].copy_from_slice(&0u32.to_be_bytes());
    bytes[16..20].copy_from_slice(&offset.to_be_bytes());
    bytes[20..24].copy_from_slice(&size.to_be_bytes());
    bytes[24..28].copy_from_slice(&12u32.to_be_bytes());
    bytes
}

#[test]
fn rejects_fat_32_bit_and_big_endian_images_explicitly() {
    let fat = plausible_fat();
    let error = relocate(Path::new("/nonexistent"), &fat, |_, _, _| {
        panic!("mapper must not run for unsupported images")
    })
    .expect_err("FAT must be unsupported");
    assert!(format!("{error:#}").contains("FAT"));

    let mut thin32 = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();
    thin32[..4].copy_from_slice(&MH_MAGIC_32_BYTES);
    assert!(format!("{:#}", parse(&thin32).unwrap_err()).contains("32-bit"));

    let mut big = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();
    big[..4].copy_from_slice(&MH_CIGAM_64_BYTES);
    assert!(format!("{:#}", parse(&big).unwrap_err()).contains("Big-endian"));

    assert!(parse(b"not mach-o").is_err());
}

#[test]
fn plans_load_id_and_rpath_changes_and_their_arguments() {
    let bytes = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, PREFIXED))
        .command(dylib_command(LC_LOAD_WEAK_DYLIB, CELLAR))
        .command(dylib_command(LC_ID_DYLIB, PREFIXED))
        .command(rpath_command("@loader_path/../lib"))
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();
    let parsed = parse(&bytes).unwrap();
    validate_relocatable(&parsed).unwrap();
    let mut observed = Vec::new();
    let plan = plan_for(&parsed, |kind, path, rpaths| {
        observed.push((
            kind,
            path.to_owned(),
            rpaths.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        ));
        placeholder_mapper(kind, path, rpaths)
    })
    .unwrap();
    assert_eq!(
        observed,
        vec![
            (
                PathKind::Dylib,
                PREFIXED.to_owned(),
                vec!["@loader_path/../lib".to_owned()]
            ),
            (
                PathKind::Dylib,
                CELLAR.to_owned(),
                vec!["@loader_path/../lib".to_owned()]
            ),
            (
                PathKind::IdDylib,
                PREFIXED.to_owned(),
                vec!["@loader_path/../lib".to_owned()]
            ),
            (
                PathKind::Rpath,
                "@loader_path/../lib".to_owned(),
                vec!["@loader_path/../lib".to_owned()]
            ),
            (
                PathKind::Dylib,
                SYSTEM_LIB.to_owned(),
                vec!["@loader_path/../lib".to_owned()]
            ),
        ]
    );
    validate_targets(&parsed, &plan.expected).unwrap();
    check_marker_plan(&bytes, &parsed, &plan.expected).unwrap();

    assert_eq!(plan.changes.len(), 3);
    assert_eq!(
        plan.changes[0],
        Change {
            kind: Kind::LoadDylib,
            old: PREFIXED.to_owned(),
            new: format!("{PREFIX}/opt/pcre2/lib/libpcre2-8.0.dylib"),
        }
    );
    assert_eq!(
        plan.changes[1],
        Change {
            kind: Kind::WeakDylib,
            old: CELLAR.to_owned(),
            new: format!("{PREFIX}/Cellar/pcre2/10.49/lib/libpcre2-8.0.dylib"),
        }
    );
    assert_eq!(
        plan.changes[2],
        Change {
            kind: Kind::IdDylib,
            old: PREFIXED.to_owned(),
            new: format!("{PREFIX}/opt/pcre2/lib/libpcre2-8.0.dylib"),
        }
    );
    assert_eq!(
        plan.expected[0].as_deref(),
        Some(plan.changes[0].new.as_str())
    );
    assert_eq!(plan.expected[4].as_deref(), Some(SYSTEM_LIB));

    assert_eq!(
        plan.arguments(),
        vec![
            "-change".to_owned(),
            PREFIXED.to_owned(),
            plan.changes[0].new.clone(),
            "-change".to_owned(),
            CELLAR.to_owned(),
            plan.changes[1].new.clone(),
            "-id".to_owned(),
            plan.changes[2].new.clone(),
        ]
    );
}

#[test]
fn clean_image_without_changes_needs_no_tools() {
    let bytes = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .command(rpath_command("@loader_path/../lib"))
        .bytes();
    let staged = TempFile::new(&bytes);
    let mut calls = 0;
    relocate(staged.path(), &bytes, |_, path, _| {
        calls += 1;
        assert!(path.starts_with('/') || path.starts_with('@'));
        Ok(None)
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(fs::read(staged.path()).unwrap(), bytes);
}

#[test]
fn rejects_markers_outside_supported_commands_before_mutation() {
    let bytes = Image::new()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .trailer(b"prefix @@HOMEBREW_PREFIX@@ in a cstring")
        .bytes();
    let staged = TempFile::new(&bytes);
    let error = relocate(staged.path(), &bytes, |_, _, _| Ok(None)).expect_err("marker must fail");
    assert!(format!("{error:#}").contains("outside a supported"));
    assert_eq!(fs::read(staged.path()).unwrap(), bytes);
}

#[test]
fn rejects_marker_inside_an_unknown_load_command() {
    let bytes = Image::new()
        .command(opaque_command(
            0x27, // LC_DYLD_ENVIRONMENT
            b"DYLD_FALLBACK_LIBRARY_PATH=@@HOMEBREW_PREFIX@@/lib",
        ))
        .bytes();
    let staged = TempFile::new(&bytes);
    let error = relocate(staged.path(), &bytes, |_, _, _| Ok(None)).expect_err("marker must fail");
    assert!(format!("{error:#}").contains("outside a supported"));
    assert_eq!(fs::read(staged.path()).unwrap(), bytes);
}

#[test]
fn rejects_marker_that_the_mapper_leaves_unresolved() {
    let bytes = Image::new()
        .command(dylib_command(LC_LOAD_DYLIB, PREFIXED))
        .bytes();
    let staged = TempFile::new(&bytes);
    let error = relocate(staged.path(), &bytes, |_, _, _| Ok(None)).expect_err("marker must fail");
    assert!(format!("{error:#}").contains("must resolve"));
    assert_eq!(fs::read(staged.path()).unwrap(), bytes);
}

#[test]
fn rejects_duplicate_ids_dylibs_and_rpaths() {
    let duplicate_id = Image::new()
        .dylib()
        .command(dylib_command(LC_ID_DYLIB, PREFIXED))
        .command(dylib_command(LC_ID_DYLIB, CELLAR))
        .bytes();
    let parsed = parse(&duplicate_id).unwrap();
    assert!(
        format!("{:#}", validate_relocatable(&parsed).unwrap_err())
            .contains("more than one LC_ID_DYLIB")
    );

    let duplicate_dylib = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .command(dylib_command(LC_LOAD_WEAK_DYLIB, SYSTEM_LIB))
        .bytes();
    let parsed = parse(&duplicate_dylib).unwrap();
    assert!(
        format!("{:#}", validate_relocatable(&parsed).unwrap_err())
            .contains("Duplicate dependent dylib")
    );

    let duplicate_rpath = Image::new()
        .dylib()
        .command(rpath_command("@loader_path/lib"))
        .command(rpath_command("@loader_path/lib"))
        .bytes();
    let parsed = parse(&duplicate_rpath).unwrap();
    assert!(
        format!("{:#}", validate_relocatable(&parsed).unwrap_err()).contains("Duplicate LC_RPATH")
    );

    let id_in_executable = Image::new()
        .command(dylib_command(LC_ID_DYLIB, PREFIXED))
        .bytes();
    let parsed = parse(&id_in_executable).unwrap();
    assert!(format!("{:#}", validate_relocatable(&parsed).unwrap_err()).contains("LC_ID_DYLIB"));
}

#[test]
fn rejects_inconsistent_or_colliding_mappings() {
    let parsed = parse_ok(
        Image::new()
            .dylib()
            .command(dylib_command(LC_LOAD_DYLIB, PREFIXED))
            .command(dylib_command(LC_LOAD_WEAK_DYLIB, PREFIXED)),
    );
    let mut calls = 0;
    let error = plan_for(&parsed, |_, _, _| {
        calls += 1;
        Ok(Some(if calls == 1 {
            format!("{PREFIX}/one.dylib")
        } else {
            format!("{PREFIX}/two.dylib")
        }))
    })
    .expect_err("inconsistent mapper must fail");
    assert!(format!("{error:#}").contains("inconsistent"));

    let parsed = parse_ok(
        Image::new()
            .dylib()
            .command(dylib_command(LC_LOAD_DYLIB, PREFIXED))
            .command(dylib_command(LC_LOAD_DYLIB, CELLAR)),
    );
    let plan = plan_for(&parsed, |_, _, _| Ok(Some(format!("{PREFIX}/same.dylib")))).unwrap();
    assert!(
        format!(
            "{:#}",
            validate_targets(&parsed, &plan.expected).unwrap_err()
        )
        .contains("share the path")
    );
}

#[test]
fn propagates_mapper_errors_with_context() {
    let parsed = parse_ok(
        Image::new()
            .dylib()
            .command(dylib_command(LC_LOAD_DYLIB, PREFIXED)),
    );
    let error = plan_for(&parsed, |_, _, _| Err(RelocateError::NotPinnedKeg)).unwrap_err();
    assert!(matches!(error, MachoError::MapPath { .. }));
    assert!(format!("{:#}", anyhow::Error::new(error)).contains("must name a pinned keg target"));
}

#[test]
fn rejects_malformed_headers_and_command_sizes() {
    let good = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();

    let mut cases: Vec<Vec<u8>> = Vec::new();
    cases.push(good[..20].to_vec());
    cases.push(good[..4].to_vec());
    cases.push(good.clone());
    cases.last_mut().unwrap()[16..20].copy_from_slice(&0u32.to_le_bytes());
    cases.push(good.clone());
    cases.last_mut().unwrap()[16..20].copy_from_slice(&(MAX_LOAD_COMMANDS + 1).to_le_bytes());
    cases.push(good.clone());
    cases.last_mut().unwrap()[20..24].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
    cases.push(good.clone());
    cases.last_mut().unwrap()[20..24].copy_from_slice(&4u32.to_le_bytes());
    cases.push(good.clone());
    cases.last_mut().unwrap()[32 + 4..32 + 8].copy_from_slice(&0u32.to_le_bytes());
    cases.push(good.clone());
    cases.last_mut().unwrap()[32 + 4..32 + 8].copy_from_slice(&12u32.to_le_bytes());
    cases.push(good.clone());
    cases.last_mut().unwrap()[32 + 4..32 + 8].copy_from_slice(&4096u32.to_le_bytes());

    for (index, bytes) in cases.iter().enumerate() {
        assert!(parse(bytes).is_err(), "case {index} should fail");
    }

    let object = Image {
        filetype: 0x1, // MH_OBJECT
        ..Image::new().command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
    };
    let bytes = object.bytes();
    let parsed = parse(&bytes).unwrap();
    assert!(format!("{:#}", validate_relocatable(&parsed).unwrap_err()).contains("file type"));

    let wrong_cpu = Image {
        cputype: 7,
        ..Image::new().command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
    };
    assert!(parse(&wrong_cpu.bytes()).is_err());
}

#[test]
fn rejects_malformed_paths_and_padding() {
    let cases = vec![
        raw_dylib_command(LC_LOAD_DYLIB, 20, b"/x", true),
        raw_dylib_command(LC_LOAD_DYLIB, 4096, b"/x", true),
        raw_dylib_command(LC_LOAD_DYLIB, 24, b"abcdefgh", false),
        raw_dylib_command(LC_LOAD_DYLIB, 24, b"", true),
        raw_dylib_command(LC_LOAD_DYLIB, 24, b"\xff", true),
        raw_dylib_command(LC_LOAD_DYLIB, 24, b"x\0Z", false),
        raw_dylib_command(LC_LOAD_DYLIB, 32, b"/x", true),
        // LC_RPATH with its path offset below the struct.
        {
            let mut command = rpath_command("@loader_path");
            command[8..12].copy_from_slice(&8u32.to_le_bytes());
            command
        },
    ];
    for (index, command) in cases.into_iter().enumerate() {
        let bytes = Image::new().dylib().command(command).bytes();
        assert!(parse(&bytes).is_err(), "case {index} should fail");
    }
}

#[test]
fn rejects_unsafe_target_names() {
    for (name, expected) in [
        ("", "empty"),
        ("-rpath", "starts with '-'"),
        ("/tmp/line\nbreak", "control character"),
        ("@@HOMEBREW_PREFIX@@/lib/x.dylib", "placeholder"),
    ] {
        let error = validate_name(name, Kind::LoadDylib, true).unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{name:?} => {error:#}"
        );
    }
    assert!(validate_name("@@HOMEBREW_PREFIX@@/lib/x.dylib", Kind::LoadDylib, false).is_ok());
}

#[test]
fn verification_accepts_only_the_exact_planned_rewrite() {
    let original = parse_ok(
        Image::new()
            .dylib()
            .command(dylib_command(LC_LOAD_DYLIB, PREFIXED))
            .command(dylib_command(LC_ID_DYLIB, CELLAR))
            .command(rpath_command("@loader_path/../lib")),
    );
    let plan = plan_for(&original, placeholder_mapper).unwrap();

    let expected = Image::new()
        .dylib()
        .command(dylib_command(
            LC_LOAD_DYLIB,
            &format!("{PREFIX}/opt/pcre2/lib/libpcre2-8.0.dylib"),
        ))
        .command(dylib_command(
            LC_ID_DYLIB,
            &format!("{PREFIX}/Cellar/pcre2/10.49/lib/libpcre2-8.0.dylib"),
        ))
        .command(rpath_command("@loader_path/../lib"));
    let expected_bytes = with_signature(expected);
    verify_stage(&original, &plan, &expected_bytes).unwrap();

    let wrong_value = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .command(dylib_command(
            LC_ID_DYLIB,
            &format!("{PREFIX}/Cellar/pcre2/10.49/lib/libpcre2-8.0.dylib"),
        ))
        .command(rpath_command("@loader_path/../lib"))
        .bytes();
    assert!(
        format!(
            "{:#}",
            verify_stage(&original, &plan, &wrong_value).unwrap_err()
        )
        .contains("expected path")
    );

    let extra_command = Image::new()
        .dylib()
        .command(dylib_command(
            LC_LOAD_DYLIB,
            &format!("{PREFIX}/opt/pcre2/lib/libpcre2-8.0.dylib"),
        ))
        .command(dylib_command(
            LC_ID_DYLIB,
            &format!("{PREFIX}/Cellar/pcre2/10.49/lib/libpcre2-8.0.dylib"),
        ))
        .command(rpath_command("@loader_path/../lib"))
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();
    assert!(
        format!(
            "{:#}",
            verify_stage(&original, &plan, &extra_command).unwrap_err()
        )
        .contains("unexpected load command")
    );

    let missing_command = Image::new()
        .dylib()
        .command(dylib_command(
            LC_LOAD_DYLIB,
            &format!("{PREFIX}/opt/pcre2/lib/libpcre2-8.0.dylib"),
        ))
        .command(rpath_command("@loader_path/../lib"))
        .bytes();
    assert!(
        format!(
            "{:#}",
            verify_stage(&original, &plan, &missing_command).unwrap_err()
        )
        .contains("changed type")
    );

    let residual = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, PREFIXED))
        .command(dylib_command(LC_ID_DYLIB, CELLAR))
        .command(rpath_command("@loader_path/../lib"))
        .bytes();
    assert!(
        format!(
            "{:#}",
            verify_stage(&original, &plan, &residual).unwrap_err()
        )
        .contains("raw Homebrew placeholder")
    );
}

#[test]
fn recognizes_every_standard_dylib_variant_and_rewrites_rpaths() {
    for (command_value, kind, label) in [
        (LC_LOAD_DYLIB, Kind::LoadDylib, "load"),
        (LC_LOAD_WEAK_DYLIB, Kind::WeakDylib, "weak"),
        (LC_REEXPORT_DYLIB, Kind::ReexportDylib, "reexport"),
        (LC_LOAD_UPWARD_DYLIB, Kind::UpwardDylib, "upward"),
        (LC_LAZY_LOAD_DYLIB, Kind::LazyDylib, "lazy"),
    ] {
        let old = format!("@@HOMEBREW_PREFIX@@/lib/{label}.dylib");
        let parsed = parse_ok(
            Image::new()
                .dylib()
                .command(dylib_command(command_value, &old)),
        );
        validate_relocatable(&parsed).unwrap();
        let plan = plan_for(&parsed, placeholder_mapper).unwrap();
        assert_eq!(plan.changes.len(), 1, "{label}");
        assert_eq!(plan.changes[0].kind, kind, "{label}");
        assert_eq!(
            plan.changes[0].new,
            format!("{PREFIX}/lib/{label}.dylib"),
            "{label}"
        );
        assert_eq!(
            plan.arguments(),
            vec![
                "-change".to_owned(),
                old.clone(),
                plan.changes[0].new.clone(),
            ],
            "{label}"
        );
    }

    let old_rpath = "@@HOMEBREW_PREFIX@@/lib";
    let parsed = parse_ok(Image::new().dylib().command(rpath_command(old_rpath)));
    validate_relocatable(&parsed).unwrap();
    let plan = plan_for(&parsed, placeholder_mapper).unwrap();
    assert_eq!(
        plan.changes,
        vec![Change {
            kind: Kind::Rpath,
            old: old_rpath.to_owned(),
            new: format!("{PREFIX}/lib"),
        }]
    );
    assert_eq!(
        plan.arguments(),
        vec![
            "-rpath".to_owned(),
            old_rpath.to_owned(),
            format!("{PREFIX}/lib"),
        ]
    );
}

#[test]
fn recognizes_64_bit_and_byte_swapped_fat_headers() {
    let mut fat64 = vec![0u8; 0x1008];
    fat64[..4].copy_from_slice(&FAT_MAGIC_64_BYTES);
    fat64[4..8].copy_from_slice(&1u32.to_be_bytes());
    fat64[8..12].copy_from_slice(&CPU_TYPE_ARM64.to_be_bytes());
    fat64[12..16].copy_from_slice(&0u32.to_be_bytes());
    fat64[16..24].copy_from_slice(&0x1000u64.to_be_bytes());
    fat64[24..32].copy_from_slice(&8u64.to_be_bytes());
    fat64[32..36].copy_from_slice(&12u32.to_be_bytes());
    assert!(is_macho(&fat64));
    assert!(format!("{:#}", parse(&fat64).unwrap_err()).contains("FAT"));

    let mut fat_le = vec![0u8; 0x1008];
    fat_le[..4].copy_from_slice(&FAT_CIGAM_BYTES);
    fat_le[4..8].copy_from_slice(&1u32.to_le_bytes());
    fat_le[8..12].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
    fat_le[12..16].copy_from_slice(&0u32.to_le_bytes());
    fat_le[16..20].copy_from_slice(&0x1000u32.to_le_bytes());
    fat_le[20..24].copy_from_slice(&8u32.to_le_bytes());
    fat_le[24..28].copy_from_slice(&12u32.to_le_bytes());
    assert!(is_macho(&fat_le));
    assert!(format!("{:#}", parse(&fat_le).unwrap_err()).contains("FAT"));

    let mut overlapping = fat64.clone();
    overlapping[4..8].copy_from_slice(&2u32.to_be_bytes());
    overlapping[8..12].copy_from_slice(&CPU_TYPE_ARM64.to_be_bytes());
    overlapping[16..24].copy_from_slice(&0x1000u64.to_be_bytes());
    overlapping[24..32].copy_from_slice(&8u64.to_be_bytes());
    overlapping[32..36].copy_from_slice(&12u32.to_be_bytes());
    overlapping[40..44].copy_from_slice(&CPU_TYPE_X86_64.to_be_bytes());
    overlapping[48..56].copy_from_slice(&0x1000u64.to_be_bytes());
    overlapping[56..64].copy_from_slice(&8u64.to_be_bytes());
    overlapping[64..68].copy_from_slice(&12u32.to_be_bytes());
    assert!(!is_macho(&overlapping));
}

#[test]
fn rejects_a_rewritten_header_with_a_different_identity() {
    let original = parse_ok(
        Image::new()
            .dylib()
            .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB)),
    );
    let expected = vec![Some(SYSTEM_LIB.to_owned())];
    let changed = Image {
        cputype: CPU_TYPE_X86_64,
        ..Image::new()
            .dylib()
            .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
    }
    .bytes();
    let changed = parse(&changed).unwrap();
    assert!(
        format!(
            "{:#}",
            compare_rewrite(&original, &expected, &changed).unwrap_err()
        )
        .contains("identity changed")
    );
}

#[test]
fn signing_preserves_metadata_only_for_signed_originals() {
    assert_eq!(
        signing_arguments(false),
        vec!["--force".to_owned(), "--sign".to_owned(), "-".to_owned()]
    );
    assert_eq!(
        signing_arguments(true),
        vec![
            "--force".to_owned(),
            "--sign".to_owned(),
            "-".to_owned(),
            "--preserve-metadata=entitlements,requirements,flags,runtime".to_owned(),
        ]
    );

    let unsigned = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();
    assert!(!parse(&unsigned).unwrap().signed);

    let signed = with_signature(
        Image::new()
            .dylib()
            .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB)),
    );
    assert!(parse(&signed).unwrap().signed);
}

#[test]
fn scanner_finds_every_marker_without_overlapping() {
    let mut bytes = b"aa@@HOMEBREW_PREFIX@@bb".to_vec();
    bytes.extend_from_slice(b"@@HOMEBREW_CELLAR@@");
    let mut found = Vec::new();
    scan_markers(&bytes, |offset| {
        found.push(offset);
        Ok(())
    })
    .unwrap();
    assert_eq!(found, vec![2, 23]);
}

#[test]
fn malformed_images_are_typed_errors() {
    assert!(matches!(parse(&[0xcf, 0xfa]), Err(MachoError::TooSmall)));

    let mut truncated = vec![0_u8; 16];
    truncated[..4].copy_from_slice(&MH_MAGIC_64_BYTES);
    assert!(matches!(
        parse(&truncated),
        Err(MachoError::TruncatedHeader)
    ));

    let mut image = Image::new()
        .dylib()
        .command(dylib_command(LC_LOAD_DYLIB, SYSTEM_LIB))
        .bytes();
    image[..4].copy_from_slice(&MH_MAGIC_32_BYTES);
    assert!(matches!(parse(&image), Err(MachoError::Bit32)));
}
