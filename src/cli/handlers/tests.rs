use super::{analytics_days, analytics_rows, handle_clean};
use crate::api::AnalyticsItem;
use crate::config::Config;
use anyhow::Result;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn config_with_cache(cache_dir: &Path) -> Config {
    let mut config = Config::default();
    config.storage.cache_dir = cache_dir.to_path_buf();
    config.storage.cache_retention = 30;
    config
}

fn seed_cache(cache_dir: &Path) -> Result<()> {
    fs::create_dir_all(cache_dir.join("catalogs"))?;
    fs::create_dir_all(cache_dir.join("bottles"))?;
    fs::write(cache_dir.join("catalogs/formulae.json"), b"[]")?;
    fs::write(cache_dir.join("catalogs/casks.json"), b"[]")?;
    fs::write(cache_dir.join("bottles/bottle.tar.gz"), b"bottle")?;
    Ok(())
}

#[test]
fn accepts_supported_periods() {
    for (period, days) in [("30d", 30), ("90d", 90), ("365d", 365), ("30", 30)] {
        assert_eq!(analytics_days(period).unwrap(), days);
    }
}

#[test]
fn rejects_unsupported_periods() {
    for period in ["7d", "0d", "year", ""] {
        assert!(
            analytics_days(period).is_err(),
            "expected {period:?} to fail"
        );
    }
}

#[test]
fn analytics_rows_build_ranked_top_rows() {
    let items = vec![
        AnalyticsItem {
            number: 0,
            formula: "alpha".to_string(),
            count: "10".to_string(),
            percent: "40".to_string(),
        },
        AnalyticsItem {
            number: 7,
            formula: "beta".to_string(),
            count: "5".to_string(),
            percent: "20%".to_string(),
        },
        AnalyticsItem {
            number: 8,
            formula: "gamma".to_string(),
            count: "2".to_string(),
            percent: "8".to_string(),
        },
    ];

    let rows = analytics_rows(&items, 2);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec!["1", "alpha", "10", "40%"]);
    assert_eq!(rows[1], vec!["7", "beta", "5", "20%"]);
    assert!(analytics_rows(&items, 0).is_empty());
}

#[tokio::test]
async fn clean_removes_catalogs_and_bottles() -> Result<()> {
    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    seed_cache(&cache_dir)?;

    handle_clean(config_with_cache(&cache_dir), true, false).await?;

    assert!(!cache_dir.join("catalogs/formulae.json").exists());
    assert!(!cache_dir.join("catalogs/casks.json").exists());
    assert!(!cache_dir.join("bottles/bottle.tar.gz").exists());
    Ok(())
}

#[tokio::test]
async fn clean_dry_run_leaves_the_cache_untouched() -> Result<()> {
    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    seed_cache(&cache_dir)?;

    handle_clean(config_with_cache(&cache_dir), true, true).await?;

    assert!(cache_dir.join("catalogs/formulae.json").exists());
    assert!(cache_dir.join("catalogs/casks.json").exists());
    assert!(cache_dir.join("bottles/bottle.tar.gz").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn clean_refuses_symlinked_bottles_before_removing_catalogs() -> Result<()> {
    use std::os::unix::fs::symlink;

    let temp = tempdir()?;
    let cache_dir = temp.path().join("cache");
    seed_cache(&cache_dir)?;
    let target = temp.path().join("target");
    fs::create_dir_all(&target)?;
    let precious = target.join("precious.tar.gz");
    fs::write(&precious, b"keep me")?;

    fs::remove_dir_all(cache_dir.join("bottles"))?;
    symlink(&target, cache_dir.join("bottles"))?;

    let result = handle_clean(config_with_cache(&cache_dir), true, false).await;

    assert!(
        result.is_err(),
        "symlinked bottles directory must be refused"
    );
    assert!(precious.exists());
    // Validation happens before any deletion, so the catalogs are intact.
    assert!(cache_dir.join("catalogs/formulae.json").exists());
    assert!(cache_dir.join("catalogs/casks.json").exists());
    Ok(())
}
