use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::Config;

use super::consts::{BOTTLE_DIR_NAME, IO_BUFFER_BYTES, MAX_BOTTLE_BYTES};

/// Create and validate the isolated native bottle cache directory.
pub(super) fn bottle_cache_dir(config: &Config) -> Result<PathBuf> {
    ensure_isolated_cache_dir(config, BOTTLE_DIR_NAME, "native bottle")
}

/// Create and validate an isolated private cache directory below `storage.cache_dir`.
pub(crate) fn ensure_isolated_cache_dir(
    config: &Config,
    subdir: &str,
    label: &str,
) -> Result<PathBuf> {
    let directory = config.storage.cache_dir.join(subdir);

    match fs::symlink_metadata(&directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to use '{}': the {label} directory is a symlink",
                    directory.display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "refusing to use '{}': the {label} path is not a directory",
                    directory.display()
                );
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(&directory).with_context(|| {
                format!(
                    "failed to create the {label} cache directory: {}",
                    directory.display()
                )
            })?;
            let metadata = fs::symlink_metadata(&directory).with_context(|| {
                format!(
                    "failed to inspect the {label} cache directory: {}",
                    directory.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "refusing to use '{}': the {label} directory is not a real directory",
                    directory.display()
                );
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect the {label} cache directory: {}",
                    directory.display()
                )
            });
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "failed to restrict permissions on the {label} cache directory: {}",
                directory.display()
            )
        })?;
    }

    Ok(directory)
}

/// Reuse `path` only when it is a regular file whose digest still matches.
///
/// Returns `Ok(None)` when the path is absent or held a mismatched regular
/// file that was discarded. Anything that is not a regular file is an error so
/// a symlink can never be followed out of the cache.
pub(crate) fn reuse_cached_bottle(path: &Path, digest: &str) -> Result<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect cached bottle: {}", path.display()));
        }
    };

    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to reuse '{}': the cached bottle is a symlink",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing to reuse '{}': the cached bottle is not a regular file",
            path.display()
        );
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
                        "discarded unverified cached bottle {}: {error:#}",
                        path.display()
                    );
                }
                Err(remove_error) if remove_error.kind() == io::ErrorKind::NotFound => {}
                Err(remove_error) => {
                    return Err(remove_error).with_context(|| {
                        format!(
                            "failed to discard an unverified cached bottle: {}",
                            path.display()
                        )
                    });
                }
            }
            Ok(None)
        }
    }
}

/// Stream `path` through SHA-256 and compare it with `expected`.
pub(crate) fn verify_file_digest(path: &Path, expected: &str) -> Result<()> {
    verify_file_digest_bounded(path, expected, MAX_BOTTLE_BYTES)
}

/// [`verify_file_digest`] with an explicit size limit.
pub(super) fn verify_file_digest_bounded(path: &Path, expected: &str, limit: u64) -> Result<()> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open cached bottle: {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect cached bottle: {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "refusing to reuse '{}': the cached bottle is not a regular file",
            path.display()
        );
    }
    if metadata.len() > limit {
        bail!(
            "refusing to reuse '{}': the cached bottle is {} bytes, above the {limit} byte limit",
            path.display(),
            metadata.len()
        );
    }

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read cached bottle: {}", path.display()))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("cached bottle size overflowed")?;
        if total > limit {
            bail!(
                "refusing to reuse '{}': the cached bottle exceeds the {limit} byte limit",
                path.display()
            );
        }
        hasher.update(&buffer[..read]);
    }

    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        bail!(
            "SHA-256 mismatch for '{}': expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}
