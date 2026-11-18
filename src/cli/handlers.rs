use anyhow::{Context, Result, bail};

use crate::api::BrewApiClient;
use crate::brew::Brew;
use crate::cache::CacheManager;
use crate::cli::ConfigAction;
use crate::config::{Config, ConfigEditor, ConfigLoader};
use crate::ui::DnfStyleFormatter;

/// Handle clean command
pub async fn handle_clean(config: Config, all: bool, dry_run: bool) -> Result<()> {
    let formatter = DnfStyleFormatter::new(&config);

    // Validate the legacy bottle directory before deleting anything, so an
    // unsafe path cannot leave the cleanup half-done. Catalog cleanup performs
    // the same check for its own directory before it removes a file.
    let cache = CacheManager::new(&config)?;

    if dry_run {
        let scope = if all { "all" } else { "expired" };
        println!(
            "Would clean {scope} brew-rs cache entries in {} (Homebrew catalogs and legacy bottle files).",
            config.storage.cache_dir.display()
        );
        return Ok(());
    }

    let (catalog_files, catalog_bytes) = crate::api::clean_catalogs(&config, all).await?;
    let bottles = if all {
        cache.clean_all().await?
    } else {
        cache.clean_expired().await?
    };

    println!("{}", formatter.header("Cache Cleanup Summary"));
    println!(
        "{}",
        formatter.field(
            "Cache directory",
            &config.storage.cache_dir.display().to_string()
        )
    );

    let rows = vec![
        vec![
            "Homebrew catalogs".to_string(),
            catalog_files.to_string(),
            formatter.format_size(catalog_bytes),
        ],
        vec![
            "Legacy bottles".to_string(),
            bottles.bottles_removed.to_string(),
            formatter.format_size(bottles.bytes_freed),
        ],
    ];
    println!(
        "{}",
        formatter.table(&["Cache", "Files removed", "Disk space freed"], &rows)
    );

    let files_removed = catalog_files + bottles.bottles_removed;
    let bytes_freed = catalog_bytes + bottles.bytes_freed;
    if files_removed == 0 {
        println!("\n{}", formatter.info("Nothing to clean."));
    } else {
        println!(
            "\n{}",
            formatter.info(&format!(
                "Removed {files_removed} files, freeing {}.",
                formatter.format_size(bytes_freed)
            ))
        );
    }

    Ok(())
}

/// Handle config command
pub async fn handle_config(mut config: Config, action: ConfigAction) -> Result<()> {
    let formatter = DnfStyleFormatter::new(&config);

    match action {
        ConfigAction::Show => {
            println!("{}", formatter.header("Current Configuration"));
            println!("{}", toml::to_string_pretty(&config)?);
        }
        ConfigAction::Edit => {
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
            let config_path = ConfigLoader::config_path()?;

            println!("Opening {} in {}...", config_path.display(), editor);
            let status = std::process::Command::new(&editor)
                .arg(&config_path)
                .status()
                .with_context(|| format!("Failed to launch editor '{}'", editor))?;
            if !status.success() {
                bail!("Editor '{}' exited with {}", editor, status);
            }
        }
        ConfigAction::Set { key, value } => {
            println!("Setting {} = {}", key, value);
            ConfigEditor::set_value_by_path(&mut config, &key, &value)?;
            ConfigEditor::save_config(&config)?;
            println!("{}", formatter.success("Configuration updated"));
        }
        ConfigAction::Reset => {
            config = Config::default();
            ConfigLoader::save(&config)?;
            println!("{}", formatter.success("Configuration reset to defaults"));
        }
    }
    Ok(())
}

/// Handle deps command
pub async fn handle_deps(brew: Brew, package: &str, tree: bool, reverse: bool) -> Result<()> {
    let heading = match (reverse, tree) {
        (true, true) => format!("Installed packages that depend on {} (recursive):", package),
        (true, false) => format!("Installed packages that depend on {}:", package),
        (false, true) => format!("Dependency tree for {}:", package),
        (false, false) => format!("Dependencies for {}:", package),
    };
    println!("{}", heading);

    if reverse {
        let mut args = vec!["--installed"];
        if tree {
            args.push("--recursive");
        }
        args.extend(["--", package]);
        brew.run("uses", &args, true)
            .await
            .with_context(|| format!("Failed to list dependents of '{}'", package))?;
    } else {
        let mut args = Vec::new();
        if tree {
            args.push("--tree");
        }
        args.extend(["--", package]);
        brew.run("deps", &args, true)
            .await
            .with_context(|| format!("Failed to list dependencies of '{}'", package))?;
    }

    Ok(())
}

/// Handle analytics command
pub async fn handle_analytics(
    config: Config,
    brew: Brew,
    package: Option<String>,
    period: String,
    top: usize,
) -> Result<()> {
    let formatter = DnfStyleFormatter::new(&config);
    let days = analytics_days(&period)?;

    if let Some(package) = package {
        println!(
            "{}",
            formatter.header(&format!("Analytics for {}", package))
        );
        println!("Period: last {} days\n", days);
        let days_arg = days.to_string();
        brew.run(
            "info",
            &[
                "--analytics",
                "--days",
                days_arg.as_str(),
                "--",
                package.as_str(),
            ],
            true,
        )
        .await
        .with_context(|| format!("Failed to load analytics for '{}'", package))?;
        return Ok(());
    }

    let client = BrewApiClient::new(&config)?;
    let analytics = client
        .get_analytics("install", &period)
        .await
        .with_context(|| {
            format!(
                "Failed to fetch global analytics for the last {} days",
                days
            )
        })?;

    let items = analytics.items.unwrap_or_default();
    if items.is_empty() {
        println!(
            "{} No analytics data available for the last {} days.",
            formatter.warning("Warning:"),
            days
        );
        return Ok(());
    }

    let rows: Vec<Vec<String>> = items
        .iter()
        .take(top)
        .enumerate()
        .map(|(index, item)| {
            let rank = if item.number == 0 {
                index + 1
            } else {
                item.number as usize
            };
            let percent = if item.percent.ends_with('%') {
                item.percent.clone()
            } else {
                format!("{}%", item.percent)
            };
            vec![
                rank.to_string(),
                item.formula.clone(),
                item.count.clone(),
                percent,
            ]
        })
        .collect();

    println!(
        "{}",
        formatter.header(&format!(
            "Top {} packages by installs (last {} days)",
            rows.len(),
            days
        ))
    );
    println!(
        "{}",
        formatter.table(&["#", "Package", "Installs", "Percent"], &rows)
    );

    if analytics.total_count > 0 {
        println!(
            "\n{}",
            formatter.info(&format!(
                "Total installs across all packages: {}",
                analytics.total_count
            ))
        );
    }

    Ok(())
}

/// Convert a CLI period such as `30d` into the day count understood by
/// `brew info --analytics`.
fn analytics_days(period: &str) -> Result<u32> {
    let raw = period.strip_suffix('d').unwrap_or(period);
    let days: u32 = raw
        .parse()
        .with_context(|| format!("Invalid analytics period '{}'", period))?;

    match days {
        30 | 90 | 365 => Ok(days),
        _ => bail!(
            "Unsupported analytics period '{}'. Use 30d, 90d or 365d.",
            period
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{analytics_days, handle_clean};
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

    #[tokio::test]
    async fn clean_removes_catalogs_and_legacy_bottles() -> Result<()> {
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
}
