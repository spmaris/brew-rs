use super::*;
use anyhow::{Context, Result};
use std::time::Duration;
use tempfile::tempdir;

fn write_file(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents)?;
    Ok(())
}

fn age_file(path: &Path, days: u64) -> Result<()> {
    let file = fs::File::options().write(true).open(path)?;
    let modified = SystemTime::now()
        .checked_sub(Duration::from_secs(days * 24 * 60 * 60))
        .context("timestamp underflow")?;
    file.set_modified(modified)?;
    Ok(())
}

#[tokio::test]
async fn clean_all_removes_regular_bottles_and_counts_bytes() -> Result<()> {
    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    let bottles = cache_dir.join("bottles");
    fs::create_dir_all(&bottles)?;
    write_file(&bottles.join("a.tar.gz"), b"alpha")?;
    write_file(&bottles.join("b.tar.gz"), b"beta!")?;

    let store = BottleStore::new(&cache_dir)?;
    let stats = store.clean_all().await?;

    assert_eq!(stats.bottles_removed, 2);
    assert_eq!(stats.bytes_freed, 10);
    assert!(!bottles.join("a.tar.gz").exists());
    assert!(!bottles.join("b.tar.gz").exists());
    // The directory itself is kept for later cleanups.
    assert!(bottles.is_dir());
    Ok(())
}

#[tokio::test]
async fn clean_expired_keeps_fresh_bottles() -> Result<()> {
    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    let bottles = cache_dir.join("bottles");
    fs::create_dir_all(&bottles)?;
    let fresh = bottles.join("fresh.tar.gz");
    write_file(&fresh, b"fresh")?;
    let stale = bottles.join("stale.tar.gz");
    write_file(&stale, b"stale")?;
    age_file(&stale, 40)?;

    let store = BottleStore::new(&cache_dir)?;
    let stats = store.clean_expired(30).await?;

    assert_eq!(stats.bottles_removed, 1);
    assert_eq!(stats.bytes_freed, 5);
    assert!(!stale.exists());
    assert!(fresh.exists());
    Ok(())
}

#[tokio::test]
async fn zero_retention_keeps_every_bottle() -> Result<()> {
    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    let bottles = cache_dir.join("bottles");
    fs::create_dir_all(&bottles)?;
    let stale = bottles.join("stale.tar.gz");
    write_file(&stale, b"stale")?;
    age_file(&stale, 365)?;

    let store = BottleStore::new(&cache_dir)?;
    let stats = store.clean_expired(0).await?;

    assert_eq!(stats.bottles_removed, 0);
    assert_eq!(stats.bytes_freed, 0);
    assert!(stale.exists());
    Ok(())
}

#[tokio::test]
async fn missing_bottle_directory_is_a_noop() -> Result<()> {
    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");

    let store = BottleStore::new(&cache_dir)?;
    assert_eq!(store.clean_all().await?, CleanupStats::default());
    assert_eq!(store.clean_expired(30).await?, CleanupStats::default());
    // Cleaning must not create the cache.
    assert!(!cache_dir.exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_symlinked_bottle_directory() -> Result<()> {
    use std::os::unix::fs::symlink;

    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    fs::create_dir_all(&cache_dir)?;
    let target = temp.path().join("target");
    fs::create_dir_all(&target)?;
    let precious = target.join("precious.tar.gz");
    write_file(&precious, b"keep me")?;
    symlink(&target, cache_dir.join("bottles"))?;

    assert!(
        matches!(
            BottleStore::new(&cache_dir),
            Err(CacheError::SymlinkedDir { .. })
        ),
        "symlinked bottles directory must be refused"
    );

    // The real file outside the cache is untouched.
    assert!(precious.exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn skips_symlinked_entries_inside_the_bottle_directory() -> Result<()> {
    use std::os::unix::fs::symlink;

    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    let bottles = cache_dir.join("bottles");
    fs::create_dir_all(&bottles)?;
    let target = temp.path().join("outside.tar.gz");
    write_file(&target, b"do not delete")?;
    symlink(&target, bottles.join("linked.tar.gz"))?;

    let store = BottleStore::new(&cache_dir)?;
    let stats = store.clean_all().await?;

    assert_eq!(stats.bottles_removed, 0);
    assert_eq!(stats.bytes_freed, 0);
    assert!(target.exists());
    assert!(fs::symlink_metadata(bottles.join("linked.tar.gz")).is_ok());
    Ok(())
}

#[tokio::test]
async fn skips_nested_directories() -> Result<()> {
    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    let bottles = cache_dir.join("bottles");
    let nested = bottles.join("nested");
    fs::create_dir_all(&nested)?;
    let inside = nested.join("keep.tar.gz");
    write_file(&inside, b"keep")?;

    let store = BottleStore::new(&cache_dir)?;
    let stats = store.clean_all().await?;

    assert_eq!(stats.bottles_removed, 0);
    assert!(inside.exists());
    Ok(())
}
