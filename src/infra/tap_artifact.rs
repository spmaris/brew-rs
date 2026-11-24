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

use sha2::{Digest, Sha256};

use crate::domain::tap_manifest::{MAX_ARTIFACT_BYTES, is_known_arch};
use crate::domain::validate::is_lower_hex_digest;

mod error;

pub(crate) use error::TapArtifactError;

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
pub(crate) fn stage_verified(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    expected_arch: &str,
) -> Result<u64, TapArtifactError> {
    stage_verified_inner(source, destination, expected_sha256, expected_arch)
}

fn stage_verified_inner(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    expected_arch: &str,
) -> Result<u64, TapArtifactError> {
    if !is_lower_hex_digest(expected_sha256) {
        return Err(TapArtifactError::StageWithoutDigest);
    }
    if !is_known_arch(expected_arch) {
        return Err(TapArtifactError::StageUnsupportedArch {
            arch: expected_arch.to_owned(),
        });
    }

    let source_io = |action: &'static str| {
        move |error: std::io::Error| TapArtifactError::Io {
            action,
            what: "cached artifact",
            path: source.to_path_buf(),
            source: error,
        }
    };
    let metadata = fs::symlink_metadata(source).map_err(source_io("inspect"))?;
    if metadata.file_type().is_symlink() {
        return Err(TapArtifactError::StageSymlink {
            path: source.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(TapArtifactError::StageNotRegular {
            path: source.to_path_buf(),
        });
    }
    if metadata.len() == 0 || metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(TapArtifactError::StageSizeOutOfRange {
            path: source.to_path_buf(),
            size: metadata.len(),
        });
    }

    let mut input = File::open(source).map_err(source_io("open"))?;
    let input_metadata = input.metadata().map_err(source_io("inspect"))?;
    if !input_metadata.is_file() || input_metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(TapArtifactError::StageChangedWhileOpened {
            path: source.to_path_buf(),
        });
    }

    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| TapArtifactError::Io {
            action: "create",
            what: "staged artifact",
            path: destination.to_path_buf(),
            source: error,
        })?;

    // From this point on the destination is known to have been created by this
    // invocation. Errors before `create_new` (including an existing
    // destination) must leave a pre-existing file untouched.
    let written = (|| -> Result<u64, TapArtifactError> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
        let mut total = 0_u64;
        loop {
            let read = input.read(&mut buffer).map_err(source_io("read"))?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .ok_or(TapArtifactError::SizeOverflow)?;
            if total > MAX_ARTIFACT_BYTES {
                return Err(TapArtifactError::StageTooLarge {
                    path: source.to_path_buf(),
                    limit: MAX_ARTIFACT_BYTES,
                });
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|error| TapArtifactError::IoPath {
                    action: "write",
                    path: destination.to_path_buf(),
                    source: error,
                })?;
        }

        let actual = hex::encode(hasher.finalize());
        if actual != expected_sha256 {
            return Err(TapArtifactError::StageDigestMismatch {
                path: source.to_path_buf(),
                expected: expected_sha256.to_owned(),
                actual,
            });
        }

        output
            .sync_all()
            .map_err(|error| TapArtifactError::IoPath {
                action: "flush",
                path: destination.to_path_buf(),
                source: error,
            })?;
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
pub(crate) fn verify_artifact(
    path: &Path,
    expected_sha256: &str,
    expected_arch: &str,
) -> Result<u64, TapArtifactError> {
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
pub(crate) fn hash_regular_file(
    path: &Path,
    expected_sha256: &str,
    limit: u64,
    what: &'static str,
) -> Result<u64, TapArtifactError> {
    if !is_lower_hex_digest(expected_sha256) {
        return Err(TapArtifactError::VerifyWithoutDigest { what });
    }
    let io_error = |action: &'static str| {
        move |source: std::io::Error| TapArtifactError::Io {
            action,
            what,
            path: path.to_path_buf(),
            source,
        }
    };
    let metadata = fs::symlink_metadata(path).map_err(io_error("inspect"))?;
    if metadata.file_type().is_symlink() {
        return Err(TapArtifactError::VerifySymlink {
            what,
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(TapArtifactError::VerifyNotRegular {
            what,
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > limit {
        return Err(TapArtifactError::VerifySizeExceeded {
            what,
            path: path.to_path_buf(),
            size: metadata.len(),
            limit,
        });
    }

    let mut file = File::open(path).map_err(io_error("open"))?;
    let opened = file.metadata().map_err(io_error("inspect"))?;
    if !opened.is_file() {
        return Err(TapArtifactError::VerifyOpenedNotRegular {
            what,
            path: path.to_path_buf(),
        });
    }

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer).map_err(io_error("read"))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(TapArtifactError::SizeOverflow)?;
        if total > limit {
            return Err(TapArtifactError::VerifyStreamExceeded {
                what,
                path: path.to_path_buf(),
                limit,
            });
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        return Err(TapArtifactError::VerifyDigestMismatch {
            what,
            path: path.to_path_buf(),
            expected: expected_sha256.to_owned(),
            actual,
        });
    }
    Ok(total)
}

fn read_header_word(header: &[u8], offset: usize) -> Result<u32, TapArtifactError> {
    let bytes: [u8; 4] = header
        .get(offset..offset + 4)
        .and_then(|word| word.try_into().ok())
        .ok_or(TapArtifactError::HeaderTooShort { offset })?;
    Ok(u32::from_le_bytes(bytes))
}

/// Validate that `path` is a thin 64-bit Mach-O executable for `expected_arch`.
pub(crate) fn validate_macho(path: &Path, expected_arch: &str) -> Result<(), TapArtifactError> {
    let expected_cpu = match expected_arch {
        "arm64" => CPU_TYPE_ARM64,
        "x86_64" => CPU_TYPE_X86_64,
        other => {
            return Err(TapArtifactError::UnsupportedArch {
                arch: other.to_owned(),
            });
        }
    };

    let mut file = File::open(path).map_err(|source| TapArtifactError::Io {
        action: "open",
        what: "tap artifact",
        path: path.to_path_buf(),
        source,
    })?;
    let mut header = [0_u8; MACH_HEADER_64_BYTES];
    file.read_exact(&mut header)
        .map_err(|source| TapArtifactError::TooShort {
            path: path.to_path_buf(),
            source,
        })?;

    if FAT_MAGICS.iter().any(|magic| header.starts_with(magic)) {
        return Err(TapArtifactError::FatMacho {
            path: path.to_path_buf(),
        });
    }
    let magic = read_header_word(&header, 0)?;
    if magic != MH_MAGIC_64 {
        return Err(TapArtifactError::NotThin64 {
            path: path.to_path_buf(),
        });
    }
    let cpu_type = read_header_word(&header, 4)?;
    let file_type = read_header_word(&header, 12)?;
    if cpu_type != expected_cpu {
        return Err(TapArtifactError::CpuMismatch {
            path: path.to_path_buf(),
            cpu_type,
            arch: expected_arch.to_owned(),
        });
    }
    if file_type != MH_EXECUTE {
        return Err(TapArtifactError::NotExecutableType {
            path: path.to_path_buf(),
            file_type,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), TapArtifactError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|source| {
        TapArtifactError::MakeExecutable {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), TapArtifactError> {
    Ok(())
}

#[cfg(test)]
mod tests;
