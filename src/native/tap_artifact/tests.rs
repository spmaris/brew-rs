use super::*;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

fn macho(cpu_type: u32, file_type: u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; 64];
    bytes[0..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
    bytes[4..8].copy_from_slice(&cpu_type.to_le_bytes());
    bytes[12..16].copy_from_slice(&file_type.to_le_bytes());
    bytes
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn accepts_thin_executables_for_the_selected_architecture() {
    let temp = tempdir().unwrap();
    let arm = temp.path().join("arm64");
    fs::write(&arm, macho(CPU_TYPE_ARM64, MH_EXECUTE)).unwrap();
    assert!(validate_macho(&arm, "arm64").is_ok());
    assert!(validate_macho(&arm, "x86_64").is_err());

    let intel = temp.path().join("x86_64");
    fs::write(&intel, macho(CPU_TYPE_X86_64, MH_EXECUTE)).unwrap();
    assert!(validate_macho(&intel, "x86_64").is_ok());
    assert!(validate_macho(&intel, "arm64").is_err());
}

#[test]
fn rejects_scripts_wrong_architectures_fat_and_non_executables() {
    let temp = tempdir().unwrap();

    let script = temp.path().join("script");
    fs::write(&script, b"#!/bin/sh\necho hello\n").unwrap();
    assert!(validate_macho(&script, "arm64").is_err());

    let dylib = temp.path().join("dylib");
    fs::write(&dylib, macho(CPU_TYPE_ARM64, 0x6)).unwrap();
    assert!(validate_macho(&dylib, "arm64").is_err());

    let fat = temp.path().join("fat");
    fs::write(&fat, [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0]).unwrap();
    assert!(validate_macho(&fat, "arm64").is_err());

    let truncated = temp.path().join("truncated");
    fs::write(&truncated, [0xcf, 0xfa, 0xed, 0xfe]).unwrap();
    assert!(validate_macho(&truncated, "arm64").is_err());
}

#[test]
fn stages_verified_artifacts_with_an_executable_mode() {
    let temp = tempdir().unwrap();
    let bytes = macho(CPU_TYPE_ARM64, MH_EXECUTE);
    let source = temp.path().join("cache");
    fs::write(&source, &bytes).unwrap();
    let destination = temp.path().join("staged");

    let total = stage_verified(&source, &destination, &digest(&bytes), "arm64").unwrap();
    assert_eq!(total, bytes.len() as u64);
    assert_eq!(fs::read(&destination).unwrap(), bytes);
    assert_eq!(
        fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn an_existing_destination_is_never_removed() {
    let temp = tempdir().unwrap();
    let bytes = macho(CPU_TYPE_ARM64, MH_EXECUTE);
    let source = temp.path().join("cache");
    fs::write(&source, &bytes).unwrap();
    let destination = temp.path().join("staged");
    fs::write(&destination, b"pre-existing contents").unwrap();

    let error = stage_verified(&source, &destination, &digest(&bytes), "arm64").unwrap_err();
    assert!(format!("{error:#}").contains("failed to create staged artifact"));
    assert_eq!(fs::read(&destination).unwrap(), b"pre-existing contents");

    let missing_source = temp.path().join("missing");
    let other = temp.path().join("other-staged");
    fs::write(&other, b"also pre-existing").unwrap();
    assert!(stage_verified(&missing_source, &other, &digest(&bytes), "arm64").is_err());
    assert_eq!(fs::read(&other).unwrap(), b"also pre-existing");
}

#[test]
fn a_digest_mismatch_removes_the_staged_copy() {
    let temp = tempdir().unwrap();
    let bytes = macho(CPU_TYPE_ARM64, MH_EXECUTE);
    let source = temp.path().join("cache");
    fs::write(&source, &bytes).unwrap();
    let destination = temp.path().join("staged");

    let error = stage_verified(&source, &destination, &"a".repeat(64), "arm64").unwrap_err();
    assert!(format!("{error:#}").contains("SHA-256 mismatch"));
    assert!(!destination.exists());
}

#[test]
fn re_verification_rejects_tampered_installed_bytes() {
    let temp = tempdir().unwrap();
    let bytes = macho(CPU_TYPE_ARM64, MH_EXECUTE);
    let path = temp.path().join("installed");
    fs::write(&path, &bytes).unwrap();
    assert_eq!(
        verify_artifact(&path, &digest(&bytes), "arm64").unwrap(),
        bytes.len() as u64
    );

    fs::write(&path, macho(CPU_TYPE_X86_64, MH_EXECUTE)).unwrap();
    assert!(verify_artifact(&path, &digest(&bytes), "arm64").is_err());
}
