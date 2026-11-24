use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::cleanup::CleanupStats;
use super::error::CacheError;

/// The legacy download cache kept under `cache_dir/bottles`.
///
/// brew-rs no longer downloads bottles there, but earlier releases did, so
/// `brewrs clean` keeps removing them during migration. Binding to the store
/// never creates a directory, and cleanup only unlinks regular files that sit
/// directly inside it: symlinked directories are refused and symlinked entries
/// are skipped so removals can never escape the cache.
pub struct BottleStore {
    bottles_dir: PathBuf,
    digest_suffix: Option<&'static str>,
}

impl BottleStore {
    /// Bind to the legacy bottle directory without creating or removing
    /// anything.
    ///
    /// A symlinked bottle directory is rejected here, before the caller has
    /// cleaned anything else, because `read_dir` would otherwise follow the
    /// link and delete files outside the cache.
    pub fn new(cache_dir: &Path) -> Result<Self, CacheError> {
        Self::bind(cache_dir.join("bottles"), None)
    }

    pub fn native(cache_dir: &Path) -> Result<Self, CacheError> {
        Self::bind(cache_dir.join(super::NATIVE_BOTTLE_DIR), Some(".tar.gz"))
    }

    pub fn native_casks(cache_dir: &Path) -> Result<Self, CacheError> {
        Self::bind(cache_dir.join(super::NATIVE_CASK_DIR), Some(".download"))
    }

    fn bind(bottles_dir: PathBuf, digest_suffix: Option<&'static str>) -> Result<Self, CacheError> {
        ensure_not_symlink(&bottles_dir)?;
        Ok(Self {
            bottles_dir,
            digest_suffix,
        })
    }

    /// Remove bottle files older than `retention_days`.
    ///
    /// A retention of `0` means "keep forever", matching the configuration
    /// documentation, so nothing is removed.
    pub async fn clean_expired(&self, retention_days: u32) -> Result<CleanupStats, CacheError> {
        if retention_days == 0 {
            return Ok(CleanupStats::default());
        }

        let cutoff = current_timestamp().saturating_sub(u64::from(retention_days) * 24 * 60 * 60);
        self.clean_matching(Some(cutoff))
    }

    /// Remove every regular file in the legacy bottle directory.
    pub async fn clean_all(&self) -> Result<CleanupStats, CacheError> {
        self.clean_matching(None)
    }

    fn clean_matching(&self, cutoff: Option<u64>) -> Result<CleanupStats, CacheError> {
        // Re-check on every cleanup so a link swapped in after construction is
        // still refused.
        ensure_not_symlink(&self.bottles_dir)?;

        let Some(directory) = self.existing_directory()? else {
            return Ok(CleanupStats::default());
        };

        let mut stats = CleanupStats::default();
        let entries = fs::read_dir(&directory).map_err(|source| CacheError::ReadDir {
            path: directory.clone(),
            source,
        })?;

        for entry in entries {
            let entry = entry.map_err(|source| CacheError::ReadEntry {
                path: directory.clone(),
                source,
            })?;

            // `DirEntry::file_type` and `DirEntry::metadata` do not follow
            // symlinks, so linked files are skipped rather than unlinked or
            // measured through their target.
            let file_type = entry
                .file_type()
                .map_err(|source| CacheError::InspectEntry {
                    path: entry.path(),
                    source,
                })?;
            if !file_type.is_file()
                || self
                    .digest_suffix
                    .is_some_and(|suffix| !is_digest_archive(&entry.file_name(), suffix))
            {
                continue;
            }

            let metadata = entry
                .metadata()
                .map_err(|source| CacheError::InspectEntry {
                    path: entry.path(),
                    source,
                })?;

            if let Some(cutoff) = cutoff {
                let Some(modified) = modified_timestamp(&metadata) else {
                    continue;
                };
                if modified >= cutoff {
                    continue;
                }
            }

            let path = entry.path();
            match fs::remove_file(&path) {
                Ok(()) => {
                    stats.bottles_removed += 1;
                    stats.bytes_freed += metadata.len();
                }
                // Another process may have removed it already.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(CacheError::RemoveFile { path, source }),
            }
        }

        Ok(stats)
    }

    fn existing_directory(&self) -> Result<Option<PathBuf>, CacheError> {
        match fs::metadata(&self.bottles_dir) {
            Ok(metadata) if metadata.is_dir() => Ok(Some(self.bottles_dir.clone())),
            // A regular file at this path is not a bottle store; there is
            // nothing safe to clean.
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(CacheError::InspectDir {
                path: self.bottles_dir.clone(),
                source,
            }),
        }
    }
}

fn is_digest_archive(name: &std::ffi::OsStr, suffix: &str) -> bool {
    name.to_str()
        .and_then(|name| name.strip_suffix(suffix))
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn ensure_not_symlink(path: &Path) -> Result<(), CacheError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(CacheError::SymlinkedDir {
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CacheError::InspectPath {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn modified_timestamp(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests;
