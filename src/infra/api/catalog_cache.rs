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

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;

use super::brew_client::BrewApiClient;
use super::error::ApiError;
use super::types::{Cask, Formula};
use crate::infra::error::error_chain;

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

/// Resolve an alias or old formula name to its unique canonical `homebrew/core`
/// formula name from a loaded catalog slice.
pub fn find_formula_alias(formulae: &[Formula], query: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    if let Some(name) = unique_match(
        formulae,
        |formula| formula.name.as_str(),
        |formula| formula.aliases.iter().any(|alias| alias == query),
    ) && name != query
    {
        return Some(name);
    }
    if let Some(name) = unique_match(
        formulae,
        |formula| formula.name.as_str(),
        |formula| formula.oldnames.iter().any(|old| old == query),
    ) && name != query
    {
        return Some(name);
    }
    None
}

/// Resolve an old cask token to its unique canonical `homebrew/cask` token
/// from a loaded catalog slice.
pub fn find_cask_old_token(casks: &[Cask], query: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let token = unique_match(
        casks,
        |cask| cask.token.as_str(),
        |cask| cask.old_tokens.iter().any(|old| old == query),
    )?;
    if token == query { None } else { Some(token) }
}

fn unique_match<T, K, P>(items: &[T], key: K, predicate: P) -> Option<String>
where
    K: Fn(&T) -> &str,
    P: Fn(&T) -> bool,
{
    let mut matched: Option<&str> = None;
    for item in items {
        let id = key(item);
        if id.is_empty() || !predicate(item) {
            continue;
        }
        match matched {
            None => matched = Some(id),
            Some(previous) if previous == id => {}
            Some(_) => return None,
        }
    }
    matched.map(str::to_owned)
}

/// Names from the on-disk formula catalog, if one exists and parses. Never
/// touches the network and ignores the cache's age.
pub fn cached_formula_names(config: &Config) -> Option<Vec<String>> {
    let path = catalogs_dir(config).join(FORMULAE_FILE);
    match read_cached::<Vec<Formula>>(&path) {
        Ok(Cached::Hit { items, .. }) => Some(items.into_iter().map(|f| f.name).collect()),
        _ => None,
    }
}

/// Resolve an alias or old name using the cached formula catalog, refreshing
/// once if the cached catalog appears to predate `aliases`/`oldnames` fields.
pub async fn resolve_formula_alias(
    config: &Config,
    query: &str,
) -> Result<Option<String>, ApiError> {
    let client = BrewApiClient::new(config)?;
    let catalog = load_formulae(&client, config, CachePolicy::PreferFresh).await?;
    if let Some(canonical) = find_formula_alias(&catalog.items, query) {
        return Ok(Some(canonical));
    }
    if !catalog.items.is_empty()
        && catalog
            .items
            .iter()
            .all(|formula| formula.aliases.is_empty() && formula.oldnames.is_empty())
    {
        let refreshed = load_formulae(&client, config, CachePolicy::Refresh).await?;
        return Ok(find_formula_alias(&refreshed.items, query));
    }
    Ok(None)
}

/// Resolve an old cask token using the cached cask catalog, refreshing once if
/// the cached catalog appears to predate `old_tokens` fields.
pub async fn resolve_cask_old_token(
    config: &Config,
    query: &str,
) -> Result<Option<String>, ApiError> {
    let client = BrewApiClient::new(config)?;
    let catalog = load_casks(&client, config, CachePolicy::PreferFresh).await?;
    if let Some(canonical) = find_cask_old_token(&catalog.items, query) {
        return Ok(Some(canonical));
    }
    if !catalog.items.is_empty() && catalog.items.iter().all(|cask| cask.old_tokens.is_empty()) {
        let refreshed = load_casks(&client, config, CachePolicy::Refresh).await?;
        return Ok(find_cask_old_token(&refreshed.items, query));
    }
    Ok(None)
}

/// Load the formula catalog according to `policy`.
pub async fn load_formulae(
    client: &BrewApiClient,
    config: &Config,
    policy: CachePolicy,
) -> Result<Catalog<Vec<Formula>>, ApiError> {
    let path = catalogs_dir(config).join(FORMULAE_FILE);
    load_catalog(&path, policy, || async {
        client
            .get_all_formulae()
            .await
            .map_err(|source| ApiError::CatalogFetch {
                catalog: "formulae",
                source: Box::new(source),
            })
    })
    .await
}

/// Load the cask catalog according to `policy`.
pub async fn load_casks(
    client: &BrewApiClient,
    config: &Config,
    policy: CachePolicy,
) -> Result<Catalog<Vec<Cask>>, ApiError> {
    let path = catalogs_dir(config).join(CASKS_FILE);
    load_catalog(&path, policy, || async {
        client
            .get_all_casks()
            .await
            .map_err(|source| ApiError::CatalogFetch {
                catalog: "casks",
                source: Box::new(source),
            })
    })
    .await
}

/// Remove the fixed catalog cache files.
///
/// With `all` the files are removed unconditionally; otherwise only entries
/// older than the configured storage retention are removed. Returns the number
/// of files removed and the number of bytes they occupied.
pub async fn clean_catalogs(config: &Config, all: bool) -> Result<(usize, u64), ApiError> {
    let dir = catalogs_dir(config);

    match fs::symlink_metadata(&dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ApiError::CacheDirSymlink { path: dir });
            }
            if !metadata.is_dir() {
                return Err(ApiError::CacheDirNotDirectory { path: dir });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(source) => return Err(ApiError::InspectCacheDir { path: dir, source }),
    }

    let now = SystemTime::now();
    let mut removed = 0usize;
    let mut freed = 0u64;

    for name in CATALOG_FILES {
        let path = dir.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(ApiError::InspectCacheFile { path, source }),
        };
        if !metadata.is_file() {
            continue;
        }
        if !all && !is_expired(&metadata, config.storage.cache_retention, now) {
            continue;
        }

        let bytes = metadata.len();
        fs::remove_file(&path).map_err(|source| ApiError::RemoveCacheFile {
            path: path.clone(),
            source,
        })?;
        removed += 1;
        freed += bytes;
    }

    Ok((removed, freed))
}

fn catalogs_dir(config: &Config) -> PathBuf {
    config.storage.cache_dir.join("catalogs")
}

async fn load_catalog<T, F, Fut>(
    path: &Path,
    policy: CachePolicy,
    fetch: F,
) -> Result<Catalog<T>, ApiError>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
{
    let mut warnings = Vec::new();
    let cached = match read_cached(path) {
        Ok(cached) => cached,
        Err(error) => {
            warnings.push(format!(
                "Ignoring unreadable catalog cache {}: {}",
                path.display(),
                error_chain(&error)
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
            Cached::Missing => Err(ApiError::NoCachedCatalog {
                path: path.to_path_buf(),
            }),
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
                            "Homebrew API request failed; using stale catalog cache at {}: {}",
                            path.display(),
                            error_chain(&error)
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

fn read_cached<T: DeserializeOwned>(path: &Path) -> Result<Cached<T>, ApiError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Cached::Missing),
        Err(source) => {
            return Err(ApiError::ReadCache {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|source| ApiError::CacheTimestamp {
            path: path.to_path_buf(),
            source,
        })?;
    let items = serde_json::from_slice(&bytes).map_err(|source| ApiError::ParseCache {
        path: path.to_path_buf(),
        source,
    })?;

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
            "Could not write catalog cache {}: {}",
            path.display(),
            error_chain(&error)
        ));
    }
}

fn write_atomic<T: Serialize>(path: &Path, items: &T) -> Result<(), ApiError> {
    let parent = path.parent().ok_or(ApiError::CachePathHasNoParent)?;
    fs::create_dir_all(parent).map_err(|source| ApiError::CreateCacheDir {
        path: parent.to_path_buf(),
        source,
    })?;

    let bytes = serde_json::to_vec(items).map_err(ApiError::SerializeCache)?;
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
        .map_err(|source| ApiError::WriteCache {
            path: temp.clone(),
            source,
        })
        .and_then(|()| {
            fs::rename(&temp, path).map_err(|source| ApiError::ReplaceCache {
                path: path.to_path_buf(),
                source,
            })
        });

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests;
