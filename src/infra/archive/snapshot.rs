//! A private, digest-verified snapshot of the archive bytes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::error::ArchiveError;
use super::limits::{IO_BUFFER_BYTES, MAX_ARCHIVE_BYTES};

/// `O_NOFOLLOW | O_NONBLOCK` where known, so a swapped-in symlink is not
/// followed and a swapped-in FIFO cannot block the open. The handle is still
/// required to be a regular file below.
#[cfg(unix)]
fn no_follow_flags() -> i32 {
    #[cfg(target_os = "macos")]
    {
        0x0100 | 0x0004
    }
    #[cfg(target_os = "linux")]
    {
        0o400000 | 0o4000
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

/// Copy the archive into private staging while checking its exact bytes.
pub(super) fn verified_snapshot(
    archive: &Path,
    out: &Path,
    sha256: &str,
) -> Result<File, ArchiveError> {
    let io_error = |action: &'static str| {
        move |source: std::io::Error| ArchiveError::Io {
            action,
            path: archive.to_path_buf(),
            source,
        }
    };
    let before = fs::symlink_metadata(archive).map_err(io_error("inspect bottle archive"))?;
    if !before.file_type().is_file() {
        return Err(ArchiveError::NotRegularFile {
            path: archive.to_path_buf(),
        });
    }
    if before.len() == 0 || before.len() > MAX_ARCHIVE_BYTES {
        return Err(ArchiveError::ArchiveSizeOutOfRange {
            path: archive.to_path_buf(),
            limit: MAX_ARCHIVE_BYTES,
        });
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(no_follow_flags());
    }
    let mut file = options
        .open(archive)
        .map_err(io_error("open bottle archive"))?;
    let opened = file
        .metadata()
        .map_err(io_error("inspect opened archive"))?;
    if !opened.is_file() || opened.len() != before.len() {
        return Err(ArchiveError::ChangedWhileOpening {
            path: archive.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != before.dev() || opened.ino() != before.ino() {
            return Err(ArchiveError::ReplacedWhileOpening {
                path: archive.to_path_buf(),
            });
        }
    }

    let mut snapshot = tempfile::tempfile_in(out).map_err(|source| ArchiveError::Stream {
        action: "create a private bottle snapshot",
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(io_error("hash bottle archive"))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(ArchiveError::ArchiveSizeOverflow)?;
        if total > MAX_ARCHIVE_BYTES {
            return Err(ArchiveError::ArchiveTooLarge {
                path: archive.to_path_buf(),
                limit: MAX_ARCHIVE_BYTES,
            });
        }
        snapshot
            .write_all(&buffer[..read])
            .map_err(|source| ArchiveError::Stream {
                action: "copy the bottle snapshot",
                source,
            })?;
        hasher.update(&buffer[..read]);
    }
    if total != opened.len() {
        return Err(ArchiveError::ChangedWhileReading {
            path: archive.to_path_buf(),
        });
    }

    let actual = hex::encode(hasher.finalize());
    if actual != sha256 {
        return Err(ArchiveError::DigestMismatch {
            expected: sha256.to_owned(),
            actual,
        });
    }

    snapshot
        .seek(SeekFrom::Start(0))
        .map_err(|source| ArchiveError::Stream {
            action: "rewind the verified bottle snapshot",
            source,
        })?;
    Ok(snapshot)
}
