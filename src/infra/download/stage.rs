use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::Response;
use sha2::{Digest, Sha256};

use crate::config::Config;

use super::consts::MAX_BOTTLE_BYTES;
use super::request::chunk_timeout;

/// Stream the response into a staging file, hashing and verifying as it goes.
pub(crate) async fn stage_bottle(
    directory: &Path,
    destination: &Path,
    digest: &str,
    response: Response,
    config: &Config,
) -> Result<()> {
    if let Some(length) = response.content_length()
        && length > MAX_BOTTLE_BYTES
    {
        bail!("refusing to download a bottle larger than the {MAX_BOTTLE_BYTES} byte limit");
    }
    ensure_destination_is_not_symlink(destination)?;

    let mut staged = StagedBottle::create(directory, digest)?;
    let mut hasher = Sha256::new();
    let mut response = response;
    let mut total = 0_u64;

    loop {
        let chunk = tokio::time::timeout(chunk_timeout(config), response.chunk())
            .await
            .context("timed out while streaming the bottle")?
            .context("failed while streaming the bottle")?;
        let Some(chunk) = chunk else {
            break;
        };
        total = total
            .checked_add(chunk.len() as u64)
            .context("bottle size overflowed")?;
        if total > MAX_BOTTLE_BYTES {
            bail!("bottle download exceeded the {MAX_BOTTLE_BYTES} byte limit");
        }
        hasher.update(&chunk);
        staged
            .write_all(&chunk)
            .context("failed to write the staged bottle")?;
    }

    if total == 0 {
        bail!("bottle download produced an empty file");
    }
    let actual = hex::encode(hasher.finalize());
    if actual != digest {
        bail!("SHA-256 mismatch for the downloaded bottle: expected {digest}, got {actual}");
    }

    staged.commit(destination)?;
    sync_directory(directory);
    Ok(())
}

/// A uniquely named staging file that removes itself unless committed.
pub(super) struct StagedBottle {
    pub(super) path: PathBuf,
    file: Option<File>,
    keep: bool,
}

impl StagedBottle {
    /// Create a new private staging file in `directory`.
    pub(super) fn create(directory: &Path, digest: &str) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        for _ in 0..16 {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = directory.join(format!(
                ".{digest}.{}.{nanos}.{unique}.tmp",
                std::process::id()
            ));

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }

            match options.open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        keep: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create a staging file in {}", directory.display())
                    });
                }
            }
        }

        bail!(
            "failed to create a unique staging file in {}",
            directory.display()
        )
    }

    /// Append streamed bytes to the staging file.
    pub(super) fn write_all(&mut self, chunk: &[u8]) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.write_all(chunk),
            None => Err(io::Error::other("staged bottle file is closed")),
        }
    }

    /// Flush the staging file and atomically publish it at `destination`.
    pub(super) fn commit(mut self, destination: &Path) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.sync_all().with_context(|| {
                format!("failed to flush the staged bottle: {}", self.path.display())
            })?;
        }
        ensure_destination_is_not_symlink(destination)?;
        fs::rename(&self.path, destination).with_context(|| {
            format!("failed to publish the bottle to {}", destination.display())
        })?;
        // The rename consumed the staging path; the drop guard must not unlink
        // the published file.
        self.keep = true;
        Ok(())
    }
}

impl Drop for StagedBottle {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        drop(self.file.take());
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                log::debug!(
                    "failed to remove the staged bottle {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

/// Refuse to overwrite a symlink at a cache destination.
pub(super) fn ensure_destination_is_not_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to replace '{}': the destination is a symlink",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect '{}'", path.display())),
    }
}

/// Best-effort directory flush after an atomic publish.
pub(super) fn sync_directory(directory: &Path) {
    #[cfg(unix)]
    if let Ok(handle) = File::open(directory)
        && let Err(error) = handle.sync_all()
    {
        log::debug!(
            "failed to flush the native bottle directory {}: {error}",
            directory.display()
        );
    }
    #[cfg(not(unix))]
    let _ = directory;
}
