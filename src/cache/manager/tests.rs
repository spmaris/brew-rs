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
async fn clean_all_removes_only_published_native_archives() -> Result<()> {
    let temp = tempdir()?;
    let dir = temp.path().join(crate::cache::NATIVE_BOTTLE_DIR);
    fs::create_dir(&dir)?;
    let archive = dir.join(format!("{}.tar.gz", "a".repeat(64)));
    let partial = dir.join(format!("{}.tar.gz.part", "b".repeat(64)));
    let unrelated = dir.join("notes.tar.gz");
    fs::write(&archive, b"published")?;
    fs::write(&partial, b"in progress")?;
    fs::write(&unrelated, b"not a bottle")?;
    let cache = CacheManager::new(&config_with_cache(temp.path(), 30))?;
    let stats = cache.clean_all().await?;
    assert_eq!(stats.bottles_removed, 1);
    assert_eq!(stats.bytes_freed, 9);
    assert!(!archive.exists());
    assert!(partial.exists() && unrelated.exists());
    Ok(())
}

#[tokio::test]
async fn clean_all_removes_only_published_native_cask_downloads() -> Result<()> {
    let temp = tempdir()?;
    let dir = temp.path().join(crate::cache::NATIVE_CASK_DIR);
    fs::create_dir(&dir)?;
    let archive = dir.join(format!("{}.download", "c".repeat(64)));
    let partial = dir.join(format!("{}.download.part", "d".repeat(64)));
    let unrelated = dir.join("notes.download");
    fs::write(&archive, b"published cask")?;
    fs::write(&partial, b"in progress")?;
    fs::write(&unrelated, b"not a cask")?;
    let cache = CacheManager::new(&config_with_cache(temp.path(), 30))?;
    let stats = cache.clean_all().await?;
    assert_eq!(stats.bottles_removed, 1);
    assert_eq!(stats.bytes_freed, 14);
    assert!(!archive.exists());
    assert!(partial.exists() && unrelated.exists());
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
