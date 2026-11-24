use std::path::Path;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::domain::plan;
use crate::infra::store::{Store, reject_protected_prefix};
use crate::ui::DnfStyleFormatter;

use super::prefix_path;
use super::upgrade::{check_updates, upgrade};

pub(crate) async fn check_all_updates(
    config: &Config,
    prefix: Option<&Path>,
    formula_only: bool,
    cask_only: bool,
    greedy: bool,
) -> Result<bool> {
    if cask_only {
        return crate::commands::casks::check_updates(config, prefix, greedy, false).await;
    }
    if formula_only {
        return check_updates(config, prefix).await;
    }
    let store = Store::open(&prefix_path(prefix)?, false)?;
    let formulae = store.installed()?;
    let casks = crate::infra::cask_store::installed(&store)?;
    let formatter = DnfStyleFormatter::new(config);
    if formulae.is_empty() && casks.is_empty() {
        formatter.note("No packages installed yet.");
        return Ok(false);
    }
    let mut has_updates = false;
    if !formulae.is_empty() {
        has_updates |= check_updates(config, prefix).await?;
    }
    if !casks.is_empty() {
        has_updates |=
            crate::commands::casks::check_updates(config, prefix, greedy, !formulae.is_empty())
                .await?;
    }
    Ok(has_updates)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn upgrade_all(
    config: &Config,
    prefix: Option<&Path>,
    packages: &[String],
    formula_only: bool,
    cask_only: bool,
    greedy: bool,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if cask_only {
        return crate::commands::casks::upgrade(
            config, prefix, packages, greedy, dry_run, yes, true,
        )
        .await
        .map(|_| ());
    }
    if formula_only {
        return upgrade(config, prefix, packages, dry_run, yes, true)
            .await
            .map(|_| ());
    }
    let path = prefix_path(prefix)?;
    reject_protected_prefix(&path)?;
    let store = Store::open(&path, false)?;
    let installed_formulae = store.installed()?;
    let installed_casks = crate::infra::cask_store::installed(&store)?;
    if packages.is_empty() {
        let formatter = DnfStyleFormatter::new(config);
        if installed_formulae.is_empty() && installed_casks.is_empty() {
            formatter.note("No packages installed yet.");
            return Ok(());
        }
        let both = !installed_formulae.is_empty() && !installed_casks.is_empty();
        let mut changed = false;
        if !installed_formulae.is_empty() {
            changed |= upgrade(config, prefix, &[], dry_run, yes, !both).await?;
        }
        if !installed_casks.is_empty() {
            changed |=
                crate::commands::casks::upgrade(config, prefix, &[], greedy, dry_run, yes, !both)
                    .await?;
        }
        if both && !changed {
            formatter.note("Everything is up to date.");
        }
        return Ok(());
    }

    let mut formula_targets = Vec::new();
    let mut cask_targets = Vec::new();
    for raw in packages {
        if raw.starts_with("homebrew/core/") {
            formula_targets.push(raw.clone());
        } else if raw.starts_with("homebrew/cask/") {
            cask_targets.push(raw.clone());
        } else {
            let formula_name = plan::parse_target(raw);
            let cask_token = crate::domain::cask_plan::parse_cask_target(raw);
            let is_formula = formula_name
                .as_ref()
                .is_ok_and(|name| installed_formulae.iter().any(|r| r.name == *name));
            let is_cask = cask_token
                .as_ref()
                .is_ok_and(|token| installed_casks.iter().any(|r| r.token == *token));
            match (is_formula, is_cask) {
                (true, true) => bail!(
                    "'{raw}' is installed as both a formula and a cask; pass --formula or --cask"
                ),
                (true, false) => formula_targets.push(raw.clone()),
                (false, true) => cask_targets.push(raw.clone()),
                (false, false) => {
                    formula_name?;
                    bail!("'{raw}' is not installed in this brewrs prefix");
                }
            }
        }
    }

    let both = !formula_targets.is_empty() && !cask_targets.is_empty();
    let mut changed = false;
    if !formula_targets.is_empty() {
        changed |= upgrade(config, prefix, &formula_targets, dry_run, yes, !both).await?;
    }
    if !cask_targets.is_empty() {
        changed |= crate::commands::casks::upgrade(
            config,
            prefix,
            &cask_targets,
            greedy,
            dry_run,
            yes,
            !both,
        )
        .await?;
    }
    if both && !changed {
        DnfStyleFormatter::new(config).note("Everything is up to date.");
    }
    Ok(())
}
