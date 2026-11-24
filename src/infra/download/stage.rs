use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Response;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::domain::progress::DownloadProgress;

use super::consts::MAX_BOTTLE_BYTES;
use super::error::DownloadError;
use super::request::chunk_timeout;

/// Stream the response into a staging file, hashing and verifying as it goes.
pub(crate) async fn stage_bottle(
    directory: &Path,
    destination: &Path,
    digest: &str,
    response: Response,
    config: &Config,
    progress: &dyn DownloadProgress,
) -> Result<u64, DownloadError> {
    if let Some(length) = response.content_length()
        && length > MAX_BOTTLE_BYTES
    {
        return Err(DownloadError::BottleTooLarge);
    }
    ensure_destination_is_not_symlink(destination)?;
    progress.start(response.content_length());

    let mut staged = StagedBottle::create(directory, digest)?;
    let mut hasher = Sha256::new();
    let mut response = response;
    let mut total = 0_u64;

    loop {
        let chunk = tokio::time::timeout(chunk_timeout(config), response.chunk())
            .await
            .map_err(DownloadError::StreamTimedOut)?
            .map_err(DownloadError::Stream)?;
        let Some(chunk) = chunk else {
            break;
        };
        total = total
            .checked_add(chunk.len() as u64)
            .ok_or(DownloadError::SizeOverflow)?;
        if total > MAX_BOTTLE_BYTES {
            return Err(DownloadError::SizeLimitExceeded);
        }
        hasher.update(&chunk);
        staged
            .write_all(&chunk)
            .map_err(DownloadError::WriteStaged)?;
        progress.advance(chunk.len() as u64);
    }

    if total == 0 {
        return Err(DownloadError::EmptyDownload);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != digest {
        return Err(DownloadError::ChecksumMismatch {
            expected: digest.to_owned(),
            actual,
        });
    }

    staged.commit(destination)?;
    sync_directory(directory);
    progress.finish();
    Ok(total)
}

/// A uniquely named staging file that removes itself unless committed.
pub(super) struct StagedBottle {
    pub(super) path: PathBuf,
    file: Option<File>,
    keep: bool,
}

impl StagedBottle {
    /// Create a new private staging file in `directory`.
    pub(super) fn create(directory: &Path, digest: &str) -> Result<Self, DownloadError> {
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
                Err(source) => {
                    return Err(DownloadError::CreateStagingFile {
                        directory: directory.to_path_buf(),
                        source,
                    });
                }
            }
        }

        Err(DownloadError::StagingFileExhausted {
            directory: directory.to_path_buf(),
        })
    }

    /// Append streamed bytes to the staging file.
    pub(super) fn write_all(&mut self, chunk: &[u8]) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.write_all(chunk),
            None => Err(io::Error::other("staged bottle file is closed")),
        }
    }

    /// Flush the staging file and atomically publish it at `destination`.
    pub(super) fn commit(mut self, destination: &Path) -> Result<(), DownloadError> {
        if let Some(file) = self.file.take() {
            file.sync_all()
                .map_err(|source| DownloadError::FlushStaged {
                    path: self.path.clone(),
                    source,
                })?;
        }
        ensure_destination_is_not_symlink(destination)?;
        fs::rename(&self.path, destination).map_err(|source| DownloadError::Publish {
            destination: destination.to_path_buf(),
            source,
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
pub(super) fn ensure_destination_is_not_symlink(path: &Path) -> Result<(), DownloadError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(DownloadError::DestinationSymlink {
                path: path.to_path_buf(),
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DownloadError::InspectDestination {
            path: path.to_path_buf(),
            source,
        }),
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
