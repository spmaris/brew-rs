//! Bounded artifact verification for the explicit tap lane.
//!
//! The tap lane accepts only thin 64-bit little-endian Mach-O executables for
//! the selected macOS architecture. Nothing is executed: the header is parsed,
//! the file is copied while being hashed again, and the executable bit is set
//! on the staged copy. A cached artifact is never trusted just because an
//! earlier download named the same digest.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::tap_manifest::{MAX_ARTIFACT_BYTES, is_known_arch};

/// Size of the 64-bit Mach-O header prefix needed for validation.
const MACH_HEADER_64_BYTES: usize = 32;
/// Thin 64-bit little-endian Mach-O magic.
const MH_MAGIC_64: u32 = 0xfeed_facf;
/// Mach-O file type for a standalone executable.
const MH_EXECUTE: u32 = 0x2;
/// Mach-O CPU type for 64-bit Arm.
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
/// Mach-O CPU type for 64-bit Intel.
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
/// Universal/fat Mach-O magics, in file byte order.
const FAT_MAGICS: [[u8; 4]; 4] = [
    [0xca, 0xfe, 0xba, 0xbe],
    [0xca, 0xfe, 0xba, 0xbf],
    [0xbe, 0xba, 0xfe, 0xca],
    [0xbf, 0xba, 0xfe, 0xca],
];
/// Read/write buffer size.
const IO_BUFFER_BYTES: usize = 64 * 1024;

/// Copy a cached artifact into a private staging path while hashing it again.
///
/// The destination must not exist. On any failure the partially written
/// destination is removed. The staged file is validated as a thin Mach-O
/// executable for `expected_arch` and made executable only after its digest
/// matches.
pub(super) fn stage_verified(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    expected_arch: &str,
) -> Result<u64> {
    stage_verified_inner(source, destination, expected_sha256, expected_arch)
}

fn stage_verified_inner(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    expected_arch: &str,
) -> Result<u64> {
    if !super::store::is_lower_hex_digest(expected_sha256) {
        bail!("refusing to stage a tap artifact without a lowercase SHA-256 digest");
    }
    if !is_known_arch(expected_arch) {
        bail!("refusing to stage a tap artifact for unsupported architecture '{expected_arch}'");
    }

    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect cached artifact '{}'", source.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to stage '{}': the cached artifact is a symlink",
            source.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing to stage '{}': the cached artifact is not a regular file",
            source.display()
        );
    }
    if metadata.len() == 0 || metadata.len() > MAX_ARTIFACT_BYTES {
        bail!(
            "refusing to stage '{}': size {} is outside the supported range",
            source.display(),
            metadata.len()
        );
    }

    let mut input = File::open(source)
        .with_context(|| format!("failed to open cached artifact '{}'", source.display()))?;
    let input_metadata = input
        .metadata()
        .with_context(|| format!("failed to inspect cached artifact '{}'", source.display()))?;
    if !input_metadata.is_file() || input_metadata.len() > MAX_ARTIFACT_BYTES {
        bail!(
            "refusing to stage '{}': the cached artifact changed while it was opened",
            source.display()
        );
    }

    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| {
            format!(
                "failed to create staged artifact '{}'",
                destination.display()
            )
        })?;

    // From this point on the destination is known to have been created by this
    // invocation. Errors before `create_new` (including an existing
    // destination) must leave a pre-existing file untouched.
    let written = (|| -> Result<u64> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
        let mut total = 0_u64;
        loop {
            let read = input.read(&mut buffer).with_context(|| {
                format!("failed to read cached artifact '{}'", source.display())
            })?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .context("tap artifact size overflowed")?;
            if total > MAX_ARTIFACT_BYTES {
                bail!(
                    "refusing to stage '{}': artifact exceeds the {MAX_ARTIFACT_BYTES} byte limit",
                    source.display()
                );
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .with_context(|| format!("failed to write '{}'", destination.display()))?;
        }

        let actual = hex::encode(hasher.finalize());
        if actual != expected_sha256 {
            bail!(
                "SHA-256 mismatch for '{}': expected {expected_sha256}, got {actual}",
                source.display()
            );
        }

        output
            .sync_all()
            .with_context(|| format!("failed to flush '{}'", destination.display()))?;
        Ok(total)
    })();
    drop(output);

    let result = written.and_then(|total| {
        set_executable(destination)?;
        validate_macho(destination, expected_arch)?;
        Ok(total)
    });
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

/// Re-hash and re-validate an installed tap executable.
pub(super) fn verify_artifact(
    path: &Path,
    expected_sha256: &str,
    expected_arch: &str,
) -> Result<u64> {
    let total = hash_regular_file(
        path,
        expected_sha256,
        MAX_ARTIFACT_BYTES,
        "installed tap artifact",
    )?;
    validate_macho(path, expected_arch)?;
    Ok(total)
}

/// Stream a regular file through SHA-256 and compare it with `expected`.
pub(super) fn hash_regular_file(
    path: &Path,
    expected_sha256: &str,
    limit: u64,
    what: &str,
) -> Result<u64> {
    if !super::store::is_lower_hex_digest(expected_sha256) {
        bail!("refusing to verify {what} without a lowercase SHA-256 digest");
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {what} '{}'", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to verify {what} '{}': it is a symlink",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing to verify {what} '{}': it is not a regular file",
            path.display()
        );
    }
    if metadata.len() > limit {
        bail!(
            "refusing to verify {what} '{}': {} bytes exceeds the {limit} byte limit",
            path.display(),
            metadata.len()
        );
    }

    let mut file =
        File::open(path).with_context(|| format!("failed to open {what} '{}'", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("failed to inspect {what} '{}'", path.display()))?;
    if !opened.is_file() {
        bail!(
            "refusing to verify {what} '{}': not a regular file",
            path.display()
        );
    }

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {what} '{}'", path.display()))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("tap artifact size overflowed")?;
        if total > limit {
            bail!(
                "refusing to verify {what} '{}': it exceeds the {limit} byte limit",
                path.display()
            );
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        bail!(
            "SHA-256 mismatch for {what} '{}': expected {expected_sha256}, got {actual}",
            path.display()
        );
    }
    Ok(total)
}

/// Validate that `path` is a thin 64-bit Mach-O executable for `expected_arch`.
pub(super) fn validate_macho(path: &Path, expected_arch: &str) -> Result<()> {
    let expected_cpu = match expected_arch {
        "arm64" => CPU_TYPE_ARM64,
        "x86_64" => CPU_TYPE_X86_64,
        other => bail!("unsupported tap artifact architecture '{other}'"),
    };

    let mut file = File::open(path)
        .with_context(|| format!("failed to open tap artifact '{}'", path.display()))?;
    let mut header = [0_u8; MACH_HEADER_64_BYTES];
    file.read_exact(&mut header).with_context(|| {
        format!(
            "tap artifact '{}' is too short to be a Mach-O executable",
            path.display()
        )
    })?;

    if FAT_MAGICS.contains(&header[0..4].try_into().expect("four byte magic")) {
        bail!(
            "refusing '{}': universal (fat) Mach-O artifacts are unsupported; provide one naked executable per architecture",
            path.display()
        );
    }
    let magic = u32::from_le_bytes(header[0..4].try_into().expect("four byte magic"));
    if magic != MH_MAGIC_64 {
        bail!(
            "refusing '{}': expected a thin 64-bit little-endian Mach-O executable",
            path.display()
        );
    }
    let cpu_type = u32::from_le_bytes(header[4..8].try_into().expect("four byte cpu type"));
    let file_type = u32::from_le_bytes(header[12..16].try_into().expect("four byte file type"));
    if cpu_type != expected_cpu {
        bail!(
            "refusing '{}': Mach-O CPU type 0x{cpu_type:08x} does not match the selected {expected_arch} artifact",
            path.display()
        );
    }
    if file_type != MH_EXECUTE {
        bail!(
            "refusing '{}': Mach-O file type 0x{file_type:08x} is not a standalone executable",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to make '{}' executable", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
