//! A private, digest-verified snapshot of the archive bytes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

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
pub(super) fn verified_snapshot(archive: &Path, out: &Path, sha256: &str) -> Result<File> {
    let before = fs::symlink_metadata(archive)
        .with_context(|| format!("failed to inspect bottle archive {}", archive.display()))?;
    if !before.file_type().is_file() {
        bail!("bottle archive {} is not a regular file", archive.display());
    }
    if before.len() == 0 || before.len() > MAX_ARCHIVE_BYTES {
        bail!(
            "bottle archive {} is empty or exceeds the {MAX_ARCHIVE_BYTES} byte limit",
            archive.display()
        );
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
        .with_context(|| format!("failed to open bottle archive {}", archive.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("failed to inspect opened archive {}", archive.display()))?;
    if !opened.is_file() || opened.len() != before.len() {
        bail!(
            "bottle archive {} changed while it was being opened",
            archive.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != before.dev() || opened.ino() != before.ino() {
            bail!(
                "bottle archive {} was replaced while it was being opened",
                archive.display()
            );
        }
    }

    let mut snapshot =
        tempfile::tempfile_in(out).context("failed to create a private bottle snapshot")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to hash bottle archive {}", archive.display()))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("bottle archive size overflowed")?;
        if total > MAX_ARCHIVE_BYTES {
            bail!(
                "bottle archive {} exceeds the {MAX_ARCHIVE_BYTES} byte limit",
                archive.display()
            );
        }
        snapshot
            .write_all(&buffer[..read])
            .context("failed to copy the bottle snapshot")?;
        hasher.update(&buffer[..read]);
    }
    if total != opened.len() {
        bail!(
            "bottle archive {} changed while it was being read",
            archive.display()
        );
    }

    let actual = hex::encode(hasher.finalize());
    if actual != sha256 {
        bail!("SHA-256 mismatch for the bottle archive: expected {sha256}, got {actual}");
    }

    snapshot
        .seek(SeekFrom::Start(0))
        .context("failed to rewind the verified bottle snapshot")?;
    Ok(snapshot)
}
