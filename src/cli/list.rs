use anyhow::Result;

use crate::brew::{Brew, OutdatedPackage};
use crate::config::Config;
use crate::ui::DnfStyleFormatter;

pub(super) async fn handle_list(
    config: &Config,
    brew: Brew,
    sizes: bool,
    dates: bool,
) -> Result<()> {
    let inventory = brew.installed().await?;
    let formatter = DnfStyleFormatter::new(config);
    let mut rows = Vec::new();
    for formula in inventory.formulae {
        let name = if formula.full_name.is_empty() {
            formula.name
        } else {
            formula.full_name
        };
        let tap = formula.tap.as_deref().unwrap_or("-");
        for keg in formula.installed {
            let mut row = vec![name.clone(), keg.version, "Formula".into(), tap.into()];
            if dates {
                row.push(format_date(keg.time));
            }
            rows.push(row);
        }
    }
    for cask in inventory.casks {
        if let Some(version) = cask.installed {
            let name = cask.full_token.unwrap_or(cask.token);
            let mut row = vec![
                name,
                version,
                "Cask".into(),
                cask.tap.unwrap_or_else(|| "-".into()),
            ];
            if dates {
                row.push(format_date(cask.installed_time));
            }
            rows.push(row);
        }
    }
    if rows.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }
    rows.sort_by(|a, b| a[0].cmp(&b[0]).then_with(|| a[2].cmp(&b[2])));
    let mut headers = vec!["Package", "Version", "Type", "Tap"];
    if dates {
        headers.push("Installed (UTC)");
    }
    println!("{}", formatter.section("Installed packages"));
    println!("{}", formatter.table(&headers, &rows));
    println!("\nTotal: {} installed version(s)", rows.len());
    if sizes {
        use std::io::Write;
        println!("\n{}", formatter.header("Disk usage"));
        std::io::stdout().flush()?;
        brew.run("info", &["--sizes"], true).await?;
    }
    Ok(())
}

pub(super) async fn show_outdated(config: &Config, brew: Brew, greedy: bool) -> Result<bool> {
    let updates = brew.outdated(greedy).await?;
    let formatter = DnfStyleFormatter::new(config);
    let mut rows = Vec::with_capacity(updates.formulae.len() + updates.casks.len());
    rows.extend(
        updates
            .formulae
            .into_iter()
            .map(|item| update_row("Formula", item)),
    );
    rows.extend(
        updates
            .casks
            .into_iter()
            .map(|item| update_row("Cask", item)),
    );
    if rows.is_empty() {
        println!("No updates available.");
        return Ok(false);
    }
    rows.sort_by(|a, b| a[0].cmp(&b[0]).then_with(|| a[1].cmp(&b[1])));
    println!("{}", formatter.section("Available updates"));
    println!(
        "{}",
        formatter.table(
            &["Package", "Type", "Installed", "Available", "Status"],
            &rows
        )
    );
    let pinned = rows.iter().filter(|row| row[4] == "Pinned").count();
    println!("\nTotal: {} update(s), {} pinned", rows.len(), pinned);
    Ok(true)
}

fn update_row(kind: &str, item: OutdatedPackage) -> Vec<String> {
    vec![
        item.name,
        kind.into(),
        item.installed_versions.join(", "),
        item.current_version,
        if item.pinned { "Pinned" } else { "Ready" }.into(),
    ]
}

fn format_date(seconds: Option<i64>) -> String {
    seconds
        .filter(|s| *s > 0)
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".into())
}
