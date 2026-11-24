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

#[test]
fn resolves_unique_aliases_and_oldnames_safely() {
    let mut kubectl = formula("kubernetes-cli");
    kubectl.aliases = vec!["kubectl".into(), "kubernetes-cli@1.37".into()];
    let mut neovim = formula("neovim");
    neovim.aliases = vec!["nvim".into()];
    let mut renamed = formula("new-name");
    renamed.oldnames = vec!["legacy-name".into(), "shared-name".into()];
    let mut alias_winner = formula("alias-owner");
    alias_winner.aliases = vec!["shared-name".into()];
    let mut self_ref = formula("self-tool");
    self_ref.aliases = vec!["self-tool".into()];
    let mut dup_a = formula("first-owner");
    dup_a.aliases = vec!["ambiguous".into()];
    let mut dup_b = formula("second-owner");
    dup_b.aliases = vec!["ambiguous".into()];

    let formulae = vec![
        kubectl,
        neovim,
        renamed,
        alias_winner,
        self_ref,
        dup_a,
        dup_b,
    ];

    assert_eq!(
        find_formula_alias(&formulae, "kubectl").as_deref(),
        Some("kubernetes-cli")
    );
    assert_eq!(
        find_formula_alias(&formulae, "nvim").as_deref(),
        Some("neovim")
    );
    assert_eq!(
        find_formula_alias(&formulae, "legacy-name").as_deref(),
        Some("new-name")
    );
    assert_eq!(
        find_formula_alias(&formulae, "shared-name").as_deref(),
        Some("alias-owner")
    );
    assert_eq!(find_formula_alias(&formulae, "self-tool"), None);
    assert_eq!(find_formula_alias(&formulae, "ambiguous"), None);
    assert_eq!(find_formula_alias(&formulae, "missing"), None);
    assert_eq!(find_formula_alias(&formulae, ""), None);
}

#[test]
fn resolves_unique_cask_old_tokens_safely() {
    let mut new_cask = cask("new-app");
    new_cask.old_tokens = vec!["legacy-app".into()];
    let mut self_cask = cask("self-app");
    self_cask.old_tokens = vec!["self-app".into()];
    let mut dup_a = cask("cask-a");
    dup_a.old_tokens = vec!["shared-old".into()];
    let mut dup_b = cask("cask-b");
    dup_b.old_tokens = vec!["shared-old".into()];

    let casks = vec![new_cask, self_cask, dup_a, dup_b];
    assert_eq!(
        find_cask_old_token(&casks, "legacy-app").as_deref(),
        Some("new-app")
    );
    assert_eq!(find_cask_old_token(&casks, "self-app"), None);
    assert_eq!(find_cask_old_token(&casks, "shared-old"), None);
    assert_eq!(find_cask_old_token(&casks, "missing"), None);
    assert_eq!(find_cask_old_token(&casks, ""), None);
}
