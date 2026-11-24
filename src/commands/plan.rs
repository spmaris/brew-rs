use anyhow::Result;
use std::io::{self, Write};

use super::install;
use crate::domain::plan::{self, Memoized, PackageSource, PlannedFormula};
use crate::infra::api::catalog_cache;
use crate::infra::formula_source::HttpFormulaSource;
use crate::infra::hint_probe::SystemHintProbe;
use crate::infra::platform::default_target;
use crate::{config::Config, ui::DnfStyleFormatter, ui::errors};

/// Build a native install plan for `targets` on host tag `tag` from the
/// official formula API, with friendly hints on a not-found target.
///
/// See [`plan::resolve_with`] for the planner's contract.
pub(crate) async fn resolve(
    config: &Config,
    targets: &[String],
    tag: &str,
) -> Result<Vec<PlannedFormula>> {
    let source = HttpFormulaSource::new(config)?;
    match plan::resolve_with(&source, targets, tag).await {
        Ok(plan) => Ok(plan),
        Err(error) => Err(add_hints_online(error, config).await),
    }
}

/// [`errors::add_hints`] with the real macOS directories, cask API, and cached
/// names.
async fn add_hints_online(error: anyhow::Error, config: &Config) -> anyhow::Error {
    let Ok(probe) = SystemHintProbe::new(config) else {
        return error;
    };
    let names = catalog_cache::cached_formula_names(config);
    errors::add_hints(error, &probe, names.as_deref()).await
}

/// A read-only audit reuses metadata across independently resolved formulae.
/// The cache is scoped to one audit; readiness never writes a metadata cache.
pub(crate) struct AuditPlanner {
    source: Memoized<HttpFormulaSource>,
}

impl AuditPlanner {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            source: Memoized::new(HttpFormulaSource::read_only(config)?),
        })
    }

    pub(crate) async fn resolve(&self, name: &str, tag: &str) -> Result<Vec<PlannedFormula>> {
        plan::resolve_with(&self.source, &[name.into()], tag).await
    }
}

pub async fn run(
    config: &Config,
    packages: &[String],
    target: Option<&str>,
    fetch: bool,
) -> Result<()> {
    let tag = match target {
        Some(tag) => tag.to_owned(),
        None => default_target()?,
    };
    let plan = resolve(config, packages, &tag).await?;
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
                if package.source == PackageSource::Upstream {
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
