use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::infra::api::error::error_chain;

use super::consts::{BOTTLE_DIR_NAME, IO_BUFFER_BYTES, MAX_BOTTLE_BYTES};
use super::error::DownloadError;

/// Create and validate the isolated native bottle cache directory.
pub(super) fn bottle_cache_dir(config: &Config) -> Result<PathBuf, DownloadError> {
    ensure_isolated_cache_dir(config, BOTTLE_DIR_NAME, "native bottle")
}

/// Create and validate an isolated private cache directory below `storage.cache_dir`.
pub(crate) fn ensure_isolated_cache_dir(
    config: &Config,
    subdir: &str,
    label: &'static str,
) -> Result<PathBuf, DownloadError> {
    let directory = config.storage.cache_dir.join(subdir);

    match fs::symlink_metadata(&directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(DownloadError::CacheDirSymlink {
                    path: directory,
                    label,
                });
            }
            if !metadata.is_dir() {
                return Err(DownloadError::CacheDirNotDirectory {
                    path: directory,
                    label,
                });
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&directory).map_err(|source| DownloadError::CreateCacheDir {
                path: directory.clone(),
                label,
                source,
            })?;
            let metadata = fs::symlink_metadata(&directory).map_err(|source| {
                DownloadError::InspectCacheDir {
                    path: directory.clone(),
                    label,
                    source,
                }
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(DownloadError::CacheDirNotReal {
                    path: directory,
                    label,
                });
            }
        }
        Err(source) => {
            return Err(DownloadError::InspectCacheDir {
                path: directory,
                label,
                source,
            });
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(|source| {
            DownloadError::RestrictCacheDir {
                path: directory.clone(),
                label,
                source,
            }
        })?;
    }

    Ok(directory)
}

/// Reuse `path` only when it is a regular file whose digest still matches.
///
/// Returns `Ok(None)` when the path is absent or held a mismatched regular
/// file that was discarded. Anything that is not a regular file is an error so
/// a symlink can never be followed out of the cache.
pub(crate) fn reuse_cached_bottle(
    path: &Path,
    digest: &str,
) -> Result<Option<PathBuf>, DownloadError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DownloadError::InspectCachedBottle {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if metadata.file_type().is_symlink() {
        return Err(DownloadError::CachedBottleSymlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(DownloadError::CachedBottleNotFile {
            path: path.to_path_buf(),
        });
    }

    match verify_file_digest(path, digest) {
        Ok(()) => Ok(Some(path.to_path_buf())),
        Err(error) => {
            // The entry is a regular file inside our dedicated directory, so
            // discarding it is safe. A file that cannot be verified must never
            // be treated as complete.
            match fs::remove_file(path) {
                Ok(()) => {
                    log::debug!(
                        "discarded unverified cached bottle {}: {}",
                        path.display(),
                        error_chain(&error)
                    );
                }
                Err(remove_error) if remove_error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(DownloadError::DiscardCachedBottle {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
            Ok(None)
        }
    }
}

/// Stream `path` through SHA-256 and compare it with `expected`.
pub(crate) fn verify_file_digest(path: &Path, expected: &str) -> Result<(), DownloadError> {
    verify_file_digest_bounded(path, expected, MAX_BOTTLE_BYTES)
}

/// [`verify_file_digest`] with an explicit size limit.
pub(super) fn verify_file_digest_bounded(
    path: &Path,
    expected: &str,
    limit: u64,
) -> Result<(), DownloadError> {
    let mut file = File::open(path).map_err(|source| DownloadError::OpenCachedBottle {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file
        .metadata()
        .map_err(|source| DownloadError::InspectCachedBottle {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(DownloadError::CachedBottleNotFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > limit {
        return Err(DownloadError::CachedBottleTooLarge {
            path: path.to_path_buf(),
            len: metadata.len(),
            limit,
        });
    }

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| DownloadError::ReadCachedBottle {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(DownloadError::CachedBottleSizeOverflow)?;
        if total > limit {
            return Err(DownloadError::CachedBottleExceedsLimit {
                path: path.to_path_buf(),
                limit,
            });
        }
        hasher.update(&buffer[..read]);
    }

    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        return Err(DownloadError::CachedChecksumMismatch {
            path: path.to_path_buf(),
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}
