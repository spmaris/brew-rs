use anyhow::{Context, Result, bail};

use crate::cli::ConfigAction;
use crate::config::{Config, ConfigEditor, ConfigLoader};
use crate::infra::api::{AnalyticsItem, BrewApiClient};
use crate::infra::cache::CacheManager;
use crate::ui::DnfStyleFormatter;

/// Refresh the local search catalogs from the native API.
pub async fn refresh_catalogs(config: &Config) -> Result<()> {
    use crate::infra::api::{CachePolicy, catalog_cache};

    let client = BrewApiClient::new(config)?;
    let (formulae, casks) = tokio::try_join!(
        catalog_cache::load_formulae(&client, config, CachePolicy::Refresh),
        catalog_cache::load_casks(&client, config, CachePolicy::Refresh),
    )?;
    if !formulae.warnings.is_empty() || !casks.warnings.is_empty() {
        bail!(
            "Could not refresh the local catalogs: {}",
            formulae
                .warnings
                .into_iter()
                .chain(casks.warnings)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    println!(
        "Refreshed {} formulae and {} casks. No packages changed.",
        formulae.items.len(),
        casks.items.len()
    );
    Ok(())
}

/// Handle clean command
pub async fn handle_clean(config: Config, all: bool, dry_run: bool) -> Result<()> {
    let formatter = DnfStyleFormatter::new(&config);

    // Validate both bottle directories before deleting anything, so an
    // unsafe path cannot leave the cleanup half-done. Catalog cleanup performs
    // the same check for its own directory before it removes a file.
    let cache = CacheManager::new(&config)?;

    if dry_run {
        let scope = if all { "all" } else { "expired" };
        println!(
            "Would clean {scope} brew-rs cache entries in {} (Homebrew catalogs, bottles, and casks).",
            config.storage.cache_dir.display()
        );
        return Ok(());
    }

    let (catalog_files, catalog_bytes) = crate::infra::api::clean_catalogs(&config, all).await?;
    let bottles = if all {
        cache.clean_all().await?
    } else {
        cache.clean_expired().await?
    };

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
            "Bottle and cask archives".to_string(),
            bottles.bottles_removed.to_string(),
            formatter.format_size(bottles.bytes_freed),
        ],
    ];
    println!(
        "{}",
        formatter.table(
            "Cache cleanup",
            &["Cache", "Files removed", "Disk space freed"],
            &rows
        )
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

/// Show native install analytics for the most popular packages.
pub async fn handle_analytics(config: Config, period: String, top: usize) -> Result<()> {
    let formatter = DnfStyleFormatter::new(&config);
    let days = analytics_days(&period)?;

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

    let rows = analytics_rows(&items, top);

    println!(
        "{}",
        formatter.table(
            &format!(
                "Top {} packages by installs (last {} days)",
                rows.len(),
                days
            ),
            &["#", "Package", "Installs", "Percent"],
            &rows
        )
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

/// Build the rank, package, install count and percentage table rows.
fn analytics_rows(items: &[AnalyticsItem], top: usize) -> Vec<Vec<String>> {
    items
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
        .collect()
}

/// Convert a CLI period such as `30d` into the day count used for analytics
/// windows.
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
mod tests;
