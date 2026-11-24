use anyhow::Result;

use crate::config::Config;

use super::bottle_store::BottleStore;
use super::cleanup::CleanupStats;

/// Cache handle used by `brewrs clean`.
///
/// Catalogue metadata is owned by the API layer and cleaned through
/// [`crate::api::clean_catalogs`]. This type handles current and legacy bottle caches.
pub struct CacheManager {
    retention_days: u32,
    bottle_store: BottleStore,
    native_bottle_store: BottleStore,
    native_cask_store: BottleStore,
}

impl CacheManager {
    /// Bind to the configured cache without creating or deleting anything.
    ///
    /// Fails when either bottles directory is a symlink, before the
    /// caller starts any cleanup.
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            retention_days: config.storage.cache_retention,
            bottle_store: BottleStore::new(&config.storage.cache_dir)?,
            native_bottle_store: BottleStore::native(&config.storage.cache_dir)?,
            native_cask_store: BottleStore::native_casks(&config.storage.cache_dir)?,
        })
    }

    /// Remove bottle and cask archives older than the configured retention.
    pub async fn clean_expired(&self) -> Result<CleanupStats> {
        let legacy = self.bottle_store.clean_expired(self.retention_days).await?;
        let native = self
            .native_bottle_store
            .clean_expired(self.retention_days)
            .await?;
        let casks = self
            .native_cask_store
            .clean_expired(self.retention_days)
            .await?;
        Ok(combine(combine(legacy, native), casks))
    }

    /// Remove all recognized bottle and cask archives.
    pub async fn clean_all(&self) -> Result<CleanupStats> {
        let legacy = self.bottle_store.clean_all().await?;
        let native = self.native_bottle_store.clean_all().await?;
        let casks = self.native_cask_store.clean_all().await?;
        Ok(combine(combine(legacy, native), casks))
    }
}

fn combine(legacy: CleanupStats, native: CleanupStats) -> CleanupStats {
    CleanupStats {
        bottles_removed: legacy.bottles_removed + native.bottles_removed,
        bytes_freed: legacy.bytes_freed + native.bytes_freed,
    }
}

#[cfg(test)]
mod tests;
