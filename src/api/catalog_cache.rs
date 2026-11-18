//! Small on-disk cache for the Homebrew API catalogs used by `search`.
//!
//! The Homebrew API serves `formula.json` and `cask.json` on every request,
//! which makes a plain search pay for two large downloads. Search only needs
//! the normalized [`Formula`] and [`Cask`] values, so this module keeps the
//! already-parsed types under `<cache_dir>/catalogs/{formulae,casks}.json` and
//! reuses them for 24 hours. Freshness is derived from the file mtime so the
//! payload stays a plain typed vector and `clean` can age entries out with the
//! configured storage retention.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;

use super::brew_client::BrewApiClient;
use super::types::{Cask, Formula};

/// Cached catalogs stay fresh for one day, matching the cadence at which the
/// Homebrew API data changes in practice.
const CATALOG_FRESHNESS: Duration = Duration::from_secs(24 * 60 * 60);

const FORMULAE_FILE: &str = "formulae.json";
const CASKS_FILE: &str = "casks.json";
const CATALOG_FILES: [&str; 2] = [FORMULAE_FILE, CASKS_FILE];

/// How a catalog load should treat the on-disk cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePolicy {
    /// Reuse a fresh cache; otherwise fetch, and fall back to stale data if
    /// the network request fails.
    PreferFresh,
    /// Never touch the network. Cached data is used regardless of its age.
    Offline,
    /// Ignore cached data, fetch from the network, and rewrite the cache.
    Refresh,
}

/// A loaded catalog and the non-fatal problems encountered while loading it.
#[derive(Debug)]
pub struct Catalog<T> {
    pub items: T,
    pub warnings: Vec<String>,
}

/// Load the formula catalog according to `policy`.
pub async fn load_formulae(
    client: &BrewApiClient,
    config: &Config,
    policy: CachePolicy,
) -> Result<Catalog<Vec<Formula>>> {
    let path = catalogs_dir(config).join(FORMULAE_FILE);
    load_catalog(&path, policy, || async {
        client
            .get_all_formulae()
            .await
            .context("Failed to fetch formulae from the Homebrew API")
    })
    .await
}

/// Load the cask catalog according to `policy`.
pub async fn load_casks(
    client: &BrewApiClient,
    config: &Config,
    policy: CachePolicy,
) -> Result<Catalog<Vec<Cask>>> {
    let path = catalogs_dir(config).join(CASKS_FILE);
    load_catalog(&path, policy, || async {
        client
            .get_all_casks()
            .await
            .context("Failed to fetch casks from the Homebrew API")
    })
    .await
}

/// Remove the fixed catalog cache files.
///
/// With `all` the files are removed unconditionally; otherwise only entries
/// older than the configured storage retention are removed. Returns the number
/// of files removed and the number of bytes they occupied.
pub async fn clean_catalogs(config: &Config, all: bool) -> Result<(usize, u64)> {
    let dir = catalogs_dir(config);

    match fs::symlink_metadata(&dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "Refusing to clean catalog cache through symlinked directory {}",
                    dir.display()
                );
            }
            if !metadata.is_dir() {
                bail!("Catalog cache path {} is not a directory", dir.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect catalog cache {}", dir.display()));
        }
    }

    let now = SystemTime::now();
    let mut removed = 0usize;
    let mut freed = 0u64;

    for name in CATALOG_FILES {
        let path = dir.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect cache file {}", path.display()));
            }
        };
        if !metadata.is_file() {
            continue;
        }
        if !all && !is_expired(&metadata, config.storage.cache_retention, now) {
            continue;
        }

        let bytes = metadata.len();
        fs::remove_file(&path)
            .with_context(|| format!("Failed to remove cache file {}", path.display()))?;
        removed += 1;
        freed += bytes;
    }

    Ok((removed, freed))
}

fn catalogs_dir(config: &Config) -> PathBuf {
    config.storage.cache_dir.join("catalogs")
}

async fn load_catalog<T, F, Fut>(path: &Path, policy: CachePolicy, fetch: F) -> Result<Catalog<T>>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut warnings = Vec::new();
    let cached = match read_cached(path) {
        Ok(cached) => cached,
        Err(error) => {
            warnings.push(format!(
                "Ignoring unreadable catalog cache {}: {error:#}",
                path.display()
            ));
            Cached::Missing
        }
    };

    match policy {
        CachePolicy::Offline => match cached {
            Cached::Hit { items, fresh } => {
                if !fresh {
                    warnings.push(format!(
                        "Using cached catalog data at {} older than 24 hours because offline mode was requested",
                        path.display()
                    ));
                }
                Ok(Catalog { items, warnings })
            }
            Cached::Missing => bail!(
                "No cached catalog at {}; run a search without --offline once to populate it",
                path.display()
            ),
        },
        CachePolicy::Refresh => {
            let items = fetch().await?;
            write_cached(path, &items, &mut warnings);
            Ok(Catalog { items, warnings })
        }
        CachePolicy::PreferFresh => {
            let stale_items = match cached {
                Cached::Hit { items, fresh: true } => {
                    return Ok(Catalog { items, warnings });
                }
                Cached::Hit {
                    items,
                    fresh: false,
                } => Some(items),
                Cached::Missing => None,
            };

            match fetch().await {
                Ok(items) => {
                    write_cached(path, &items, &mut warnings);
                    Ok(Catalog { items, warnings })
                }
                Err(error) => match stale_items {
                    Some(items) => {
                        warnings.push(format!(
                            "Homebrew API request failed; using stale catalog cache at {}: {error:#}",
                            path.display()
                        ));
                        Ok(Catalog { items, warnings })
                    }
                    None => Err(error),
                },
            }
        }
    }
}

enum Cached<T> {
    Hit { items: T, fresh: bool },
    Missing,
}

fn read_cached<T: DeserializeOwned>(path: &Path) -> Result<Cached<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Cached::Missing),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };

    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .with_context(|| format!("Failed to read the timestamp of {}", path.display()))?;
    let items = serde_json::from_slice(&bytes)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    Ok(Cached::Hit {
        items,
        fresh: is_fresh(modified),
    })
}

fn is_fresh(modified: SystemTime) -> bool {
    is_fresh_at(modified, SystemTime::now())
}

fn is_fresh_at(modified: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age < CATALOG_FRESHNESS,
        // A timestamp in the future is clock skew, not evidence of a stale
        // file; treating it as fresh avoids refetching on every invocation.
        Err(_) => true,
    }
}

fn is_expired(metadata: &fs::Metadata, retention_days: u32, now: SystemTime) -> bool {
    if retention_days == 0 {
        return false;
    }
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    match now.duration_since(modified) {
        Ok(age) => age > Duration::from_secs(u64::from(retention_days) * 24 * 60 * 60),
        // Unknown or future timestamps are left alone.
        Err(_) => false,
    }
}

fn write_cached<T: Serialize>(path: &Path, items: &T, warnings: &mut Vec<String>) {
    if let Err(error) = write_atomic(path, items) {
        // A cache write must never fail a search: the fetched data is already
        // in hand, so report the problem and move on.
        warnings.push(format!(
            "Could not write catalog cache {}: {error:#}",
            path.display()
        ));
    }
}

fn write_atomic<T: Serialize>(path: &Path, items: &T) -> Result<()> {
    let parent = path
        .parent()
        .context("Catalog cache path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "Failed to create catalog cache directory {}",
            parent.display()
        )
    })?;

    let bytes = serde_json::to_vec(items).context("Failed to serialize catalog cache")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("catalog.json");
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(".{file_name}.{}.{unique}.tmp", std::process::id()));

    let result = fs::write(&temp, &bytes)
        .with_context(|| format!("Failed to write {}", temp.display()))
        .and_then(|()| {
            fs::rename(&temp, path).with_context(|| format!("Failed to replace {}", path.display()))
        });

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use tempfile::tempdir;

    fn formula(name: &str) -> Formula {
        Formula {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn cask(token: &str) -> Cask {
        Cask {
            token: token.to_string(),
            ..Default::default()
        }
    }

    fn config_with_cache(dir: &Path) -> Config {
        let mut config = Config::default();
        config.storage.cache_dir = dir.to_path_buf();
        config
    }

    /// Move a file's mtime into the past without adding a dependency.
    fn age_file(path: &Path, age: Duration) -> Result<()> {
        let file = fs::OpenOptions::new().write(true).open(path)?;
        let modified = SystemTime::now() - age;
        file.set_times(fs::FileTimes::new().set_modified(modified))?;
        Ok(())
    }

    fn write_example(path: &Path, names: &[&str]) -> Result<()> {
        let items: Vec<Formula> = names.iter().map(|name| formula(name)).collect();
        write_atomic(path, &items)
    }

    fn read_names(path: &Path) -> Result<Vec<String>> {
        let items: Vec<Formula> = serde_json::from_slice(&fs::read(path)?)?;
        Ok(items.into_iter().map(|item| item.name).collect())
    }

    #[test]
    fn freshness_is_based_on_a_24_hour_window() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(is_fresh_at(now - Duration::from_secs(23 * 3600), now));
        assert!(!is_fresh_at(now - Duration::from_secs(25 * 3600), now));
        // Future timestamps are clock skew, not staleness.
        assert!(is_fresh_at(now + Duration::from_secs(3600), now));
    }

    #[tokio::test]
    async fn fresh_cache_is_used_without_fetching() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("catalogs").join(FORMULAE_FILE);
        write_example(&path, &["cached"])?;

        let calls = Cell::new(0);
        let catalog = load_catalog(&path, CachePolicy::PreferFresh, || {
            calls.set(calls.get() + 1);
            async { Err::<Vec<Formula>, _>(anyhow::anyhow!("network must not be used")) }
        })
        .await?;

        assert_eq!(calls.get(), 0);
        assert_eq!(catalog.items[0].name, "cached");
        assert!(catalog.warnings.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn expired_cache_is_refreshed_and_rewritten() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("catalogs").join(FORMULAE_FILE);
        write_example(&path, &["old"])?;
        age_file(&path, Duration::from_secs(25 * 3600))?;

        let catalog = load_catalog(&path, CachePolicy::PreferFresh, || async {
            Ok(vec![formula("fresh")])
        })
        .await?;

        assert_eq!(catalog.items[0].name, "fresh");
        assert!(catalog.warnings.is_empty());
        assert_eq!(read_names(&path)?, vec!["fresh".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn expired_cache_is_used_when_the_network_fails() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("catalogs").join(FORMULAE_FILE);
        write_example(&path, &["cached"])?;
        age_file(&path, Duration::from_secs(25 * 3600))?;

        let catalog = load_catalog(&path, CachePolicy::PreferFresh, || async {
            Err::<Vec<Formula>, _>(anyhow::anyhow!("network down"))
        })
        .await?;

        assert_eq!(catalog.items[0].name, "cached");
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("stale")),
            "expected a stale fallback warning, got {:?}",
            catalog.warnings
        );
        Ok(())
    }

    #[tokio::test]
    async fn offline_uses_expired_cache_without_fetching() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("catalogs").join(FORMULAE_FILE);
        write_example(&path, &["cached"])?;
        age_file(&path, Duration::from_secs(25 * 3600))?;

        let calls = Cell::new(0);
        let catalog = load_catalog(&path, CachePolicy::Offline, || {
            calls.set(calls.get() + 1);
            async { Ok(vec![formula("network")]) }
        })
        .await?;

        assert_eq!(calls.get(), 0);
        assert_eq!(catalog.items[0].name, "cached");
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("older than 24 hours")),
            "expected an offline staleness warning, got {:?}",
            catalog.warnings
        );
        Ok(())
    }

    #[tokio::test]
    async fn offline_without_cache_is_an_error() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("catalogs").join(FORMULAE_FILE);

        let error = load_catalog::<Vec<Formula>, _, _>(&path, CachePolicy::Offline, || async {
            Ok(vec![formula("network")])
        })
        .await
        .unwrap_err();

        assert!(error.to_string().contains("--offline"));
        Ok(())
    }

    #[tokio::test]
    async fn refresh_ignores_and_replaces_the_cache() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("catalogs").join(FORMULAE_FILE);
        write_example(&path, &["old"])?;

        let catalog = load_catalog(&path, CachePolicy::Refresh, || async {
            Ok(vec![formula("fetched")])
        })
        .await?;

        assert_eq!(catalog.items[0].name, "fetched");
        assert_eq!(read_names(&path)?, vec!["fetched".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn cache_write_failure_does_not_fail_the_search() -> Result<()> {
        let dir = tempdir()?;
        // A file where the directory should be makes create_dir_all fail.
        let blocker = dir.path().join("catalogs");
        fs::write(&blocker, b"not a directory")?;
        let path = blocker.join(FORMULAE_FILE);

        let catalog = load_catalog(&path, CachePolicy::Refresh, || async {
            Ok(vec![formula("fetched")])
        })
        .await?;

        assert_eq!(catalog.items[0].name, "fetched");
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("Could not write catalog cache")),
            "expected a cache write warning, got {:?}",
            catalog.warnings
        );
        Ok(())
    }

    #[tokio::test]
    async fn public_loader_reads_the_fixed_cache_file() -> Result<()> {
        let dir = tempdir()?;
        let config = config_with_cache(dir.path());
        let path = catalogs_dir(&config).join(FORMULAE_FILE);
        write_example(&path, &["cached"])?;

        let client = BrewApiClient::new(&config)?;
        let catalog = load_formulae(&client, &config, CachePolicy::PreferFresh).await?;
        assert_eq!(catalog.items[0].name, "cached");

        let cask_path = catalogs_dir(&config).join(CASKS_FILE);
        write_atomic(&cask_path, &vec![cask("cached-cask")])?;
        let catalog = load_casks(&client, &config, CachePolicy::PreferFresh).await?;
        assert_eq!(catalog.items[0].token, "cached-cask");
        Ok(())
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp_files() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("catalogs").join(FORMULAE_FILE);

        write_example(&path, &["one"])?;
        write_example(&path, &["two"])?;

        assert_eq!(read_names(&path)?, vec!["two".to_string()]);
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        Ok(())
    }

    #[tokio::test]
    async fn clean_removes_expired_catalogs_only() -> Result<()> {
        let dir = tempdir()?;
        let mut config = config_with_cache(dir.path());
        config.storage.cache_retention = 30;
        let formulae = catalogs_dir(&config).join(FORMULAE_FILE);
        let casks = catalogs_dir(&config).join(CASKS_FILE);

        write_example(&formulae, &["fresh"])?;
        write_atomic(&casks, &vec![cask("stale")])?;
        age_file(&casks, Duration::from_secs(40 * 24 * 3600))?;
        let expired_bytes = fs::metadata(&casks)?.len();

        let (removed, freed) = clean_catalogs(&config, false).await?;
        assert_eq!(removed, 1);
        assert_eq!(freed, expired_bytes);
        assert!(formulae.exists());
        assert!(!casks.exists());

        let (removed, _) = clean_catalogs(&config, true).await?;
        assert_eq!(removed, 1);
        assert!(!formulae.exists());
        Ok(())
    }

    #[tokio::test]
    async fn clean_ignores_unrelated_files() -> Result<()> {
        let dir = tempdir()?;
        let config = config_with_cache(dir.path());
        let dir = catalogs_dir(&config);
        fs::create_dir_all(&dir)?;
        let unrelated = dir.join("notes.json");
        fs::write(&unrelated, b"keep me")?;

        let (removed, freed) = clean_catalogs(&config, true).await?;
        assert_eq!((removed, freed), (0, 0));
        assert!(unrelated.exists());
        Ok(())
    }

    #[tokio::test]
    async fn clean_of_a_missing_directory_is_a_no_op() -> Result<()> {
        let dir = tempdir()?;
        let config = config_with_cache(dir.path());
        assert_eq!(clean_catalogs(&config, true).await?, (0, 0));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clean_refuses_a_symlinked_catalogs_directory() -> Result<()> {
        let dir = tempdir()?;
        let outside = tempdir()?;
        let config = config_with_cache(dir.path());
        let link = catalogs_dir(&config);
        fs::create_dir_all(link.parent().unwrap())?;
        std::os::unix::fs::symlink(outside.path(), &link)?;

        let error = clean_catalogs(&config, true).await.unwrap_err();
        assert!(error.to_string().contains("symlinked"));
        Ok(())
    }
}
