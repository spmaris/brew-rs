use anyhow::Result;
use std::io::{self, Write};

use super::install;
use crate::{config::Config, domain::plan, ui::DnfStyleFormatter};

pub async fn run(
    config: &Config,
    packages: &[String],
    target: Option<&str>,
    fetch: bool,
) -> Result<()> {
    let tag = match target {
        Some(tag) => tag.to_owned(),
        None => plan::default_target()?,
    };
    let plan = plan::resolve(config, packages, &tag).await?;
    let ui = DnfStyleFormatter::new(config);
    let rows: Vec<_> = plan
        .iter()
        .map(|package| {
            vec![
                package.name.clone(),
                package.version.clone(),
                if package.requested {
                    "requested"
                } else {
                    "dependency"
                }
                .into(),
                if package.source == crate::infra::store::PackageSource::Upstream {
                    "Upstream release"
                } else if package.cellar == ":any_skip_relocation" {
                    "Portable"
                } else if package.cellar == ":any" {
                    "Relocation needed"
                } else {
                    "Fixed prefix"
                }
                .into(),
            ]
        })
        .collect();
    let rendered = ui.table_with_summary(
        if fetch {
            "Bottle fetch"
        } else {
            "Install plan"
        },
        &["Package", "Version", "Reason", "Bottle"],
        &rows,
        &[("Target", tag.clone())],
    );
    println!("{}", rendered.table);
    if let Some(summary) = rendered.summary {
        eprintln!("{summary}");
    }
    if !fetch {
        println!(
            "\n{} {} planned, nothing applied.",
            plan.len(),
            if plan.len() == 1 {
                "package"
            } else {
                "packages"
            }
        );
        return Ok(());
    }

    io::stdout().flush()?;
    for (index, package) in plan.iter().enumerate() {
        let path = install::fetch_formula(config, package).await?;
        if log::log_enabled!(log::Level::Info) {
            log::info!("Verified bottle: {}", path.display());
        }
        println!(
            "[{}/{}] {}  {} {}",
            index + 1,
            plan.len(),
            ui.success("Verified"),
            package.name,
            package.version
        );
    }
    println!("\nVerified {} bottles. No packages installed.", plan.len());
    Ok(())
}
