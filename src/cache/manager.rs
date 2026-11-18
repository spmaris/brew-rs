use anyhow::Result;

use crate::config::Config;

use super::bottle_store::BottleStore;
use super::cleanup::CleanupStats;

/// Cache handle used by `brewrs clean`.
///
/// Catalogue metadata is owned by the API layer and cleaned through
/// [`crate::api::clean_catalogs`]. This type is deliberately limited to the
/// legacy bottle directory that earlier brew-rs releases downloaded into, so
/// no in-memory formula cache is kept here anymore.
pub struct CacheManager {
    retention_days: u32,
    bottle_store: BottleStore,
}

impl CacheManager {
    /// Bind to the configured cache without creating or deleting anything.
    ///
    /// Fails when the legacy bottles directory is a symlink, before the
    /// caller starts any cleanup.
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            retention_days: config.storage.cache_retention,
            bottle_store: BottleStore::new(&config.storage.cache_dir)?,
        })
    }

    /// Remove legacy bottle files older than the configured retention.
    pub async fn clean_expired(&self) -> Result<CleanupStats> {
        self.bottle_store.clean_expired(self.retention_days).await
    }

    /// Remove every legacy bottle file.
    pub async fn clean_all(&self) -> Result<CleanupStats> {
        self.bottle_store.clean_all().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn config_with_cache(cache_dir: &Path, retention_days: u32) -> Config {
        let mut config = Config::default();
        config.storage.cache_dir = cache_dir.to_path_buf();
        config.storage.cache_retention = retention_days;
        config
    }

    #[tokio::test]
    async fn clean_all_removes_legacy_bottles_without_creating_directories() -> Result<()> {
        let temp = tempdir()?;
        let cache_dir = temp.path().join("cache");
        let bottles = cache_dir.join("bottles");
        fs::create_dir_all(&bottles)?;
        fs::write(bottles.join("bottle.tar.gz"), b"bottle")?;

        let cache = CacheManager::new(&config_with_cache(&cache_dir, 30))?;
        let stats = cache.clean_all().await?;

        assert_eq!(stats.bottles_removed, 1);
        assert_eq!(stats.bytes_freed, 6);
        assert!(!bottles.join("bottle.tar.gz").exists());
        Ok(())
    }

    #[tokio::test]
    async fn clean_expired_respects_zero_retention() -> Result<()> {
        let temp = tempdir()?;
        let cache_dir = temp.path().join("cache");
        let bottles = cache_dir.join("bottles");
        fs::create_dir_all(&bottles)?;
        fs::write(bottles.join("bottle.tar.gz"), b"bottle")?;

        let cache = CacheManager::new(&config_with_cache(&cache_dir, 0))?;
        let stats = cache.clean_expired().await?;

        assert_eq!(stats.bottles_removed, 0);
        assert!(bottles.join("bottle.tar.gz").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn new_refuses_symlinked_bottles_directory() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempdir()?;
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&cache_dir)?;
        let target = temp.path().join("target");
        fs::create_dir_all(&target)?;
        let precious = target.join("precious.tar.gz");
        fs::write(&precious, b"keep me")?;
        symlink(&target, cache_dir.join("bottles"))?;

        assert!(CacheManager::new(&config_with_cache(&cache_dir, 30)).is_err());
        assert!(precious.exists());
        Ok(())
    }
}
