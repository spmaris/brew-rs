use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::config::{Config, ConfigEditor};

use super::{
    Cli, Commands, ConfigAction, NativeAction, ServiceAction, TapAction, handlers, search,
};

impl Cli {
    pub fn load_config(&self) -> Result<Config> {
        use crate::config::ConfigLoader;

        match self.command {
            Commands::Config {
                action: ConfigAction::Reset,
            } => Ok(Config::default()),
            Commands::Config {
                action: ConfigAction::Edit,
            } if ConfigLoader::config_path()?.try_exists()? => Ok(Config::default()),
            Commands::Config {
                action: ConfigAction::Edit,
            } => ConfigLoader::load(),
            _ => ConfigLoader::load_read_only(),
        }
    }

    pub async fn execute(self, mut config: Config) -> Result<ExitCode> {
        // Display overrides must not be saved to the config file.
        if !matches!(self.command, Commands::Config { .. }) {
            if self.no_color {
                config.ui.colored = false;
            }
            if let Some(parallel) = self.parallel {
                ConfigEditor::set_parallel_downloads(&mut config, parallel)?;
            }
        }
        let prefix = self.prefix.as_deref();
        use crate::commands::install as native;

        match self.command {
            Commands::Native { action } => match action {
                NativeAction::Install {
                    packages,
                    source,
                    transaction,
                } => {
                    native::install(
                        &config,
                        prefix,
                        &packages,
                        transaction.dry_run,
                        transaction.yes,
                        source,
                    )
                    .await?;
                }
                NativeAction::Upgrade {
                    packages,
                    transaction,
                } => {
                    native::upgrade(
                        &config,
                        prefix,
                        &packages,
                        transaction.dry_run,
                        transaction.yes,
                        true,
                    )
                    .await?;
                }
                NativeAction::Remove {
                    packages,
                    autoremove,
                    transaction,
                } => {
                    native::remove(
                        &config,
                        prefix,
                        &packages,
                        transaction.dry_run,
                        transaction.yes,
                        autoremove,
                    )?;
                }
                NativeAction::Autoremove { transaction } => {
                    native::autoremove(&config, prefix, transaction.dry_run, transaction.yes)?;
                }
                NativeAction::List => native::list(&config, prefix)?,
            },
            Commands::Migrate {
                brew_prefix,
                check,
                service_dirs,
            } => {
                crate::commands::migration::audit(
                    &config,
                    prefix,
                    brew_prefix.as_deref(),
                    service_dirs,
                    check,
                )
                .await?;
            }
            Commands::Services { action } => match action {
                ServiceAction::Plan { package } => {
                    crate::commands::services::preview(&config, prefix, &package).await?;
                }
                ServiceAction::List => {
                    crate::commands::services::list(&config, prefix)?;
                }
            },
            Commands::Tap { action } => match action {
                TapAction::Add { tap, manifest } => {
                    crate::commands::tap::add(&config, prefix, &tap, &manifest)?;
                }
                TapAction::List => crate::commands::tap::list(&config, prefix)?,
                TapAction::Plan { package } => {
                    crate::commands::tap::plan(&config, prefix, &package)?;
                }
                TapAction::Install {
                    package,
                    transaction,
                } => {
                    crate::commands::tap::install(
                        &config,
                        prefix,
                        &package,
                        transaction.dry_run,
                        transaction.yes,
                    )
                    .await?;
                }
                TapAction::Uninstall {
                    package,
                    transaction,
                } => {
                    crate::commands::tap::uninstall(
                        &config,
                        prefix,
                        &package,
                        transaction.dry_run,
                        transaction.yes,
                    )?;
                }
            },
            Commands::Untap { tap } => {
                crate::commands::tap::untap(&config, prefix, &tap)?;
            }
            Commands::Shellenv { shell } => {
                native::shellenv(prefix, shell.as_deref())?;
            }
            Commands::Plan { packages, target } => {
                crate::commands::run(&config, &packages, target.as_deref(), false).await?;
            }
            Commands::Fetch { packages, target } => {
                crate::commands::run(&config, &packages, target.as_deref(), true).await?;
            }
            Commands::Install {
                packages,
                appdir,
                force,
                no_deps,
                head,
                source,
                kind,
                transaction,
            } => {
                reject_unsupported(&[
                    (head, "--head"),
                    (no_deps, "--no-deps"),
                    (force, "--force"),
                ])?;
                if kind.cask {
                    crate::commands::casks::install(
                        &config,
                        prefix,
                        appdir.as_deref(),
                        &packages,
                        transaction.dry_run,
                        transaction.yes,
                    )
                    .await?;
                } else {
                    if appdir.is_some() {
                        bail!("--appdir requires --cask; no changes were made.");
                    }
                    native::install(
                        &config,
                        prefix,
                        &packages,
                        transaction.dry_run,
                        transaction.yes,
                        source,
                    )
                    .await?;
                }
            }
            Commands::Remove {
                packages,
                autoremove,
                force,
                kind,
                transaction,
            } => {
                if kind.cask {
                    if autoremove {
                        bail!("--autoremove is not supported with --cask; no changes were made.");
                    }
                    crate::commands::casks::remove(
                        &config,
                        prefix,
                        &packages,
                        transaction.dry_run,
                        transaction.yes || force,
                    )?;
                } else {
                    native::remove(
                        &config,
                        prefix,
                        &packages,
                        transaction.dry_run,
                        transaction.yes || force,
                        autoremove,
                    )?;
                }
            }
            Commands::Autoremove { transaction } => {
                native::autoremove(&config, prefix, transaction.dry_run, transaction.yes)?;
            }
            Commands::Search {
                query,
                limit,
                kind,
                offline,
                refresh,
            } => {
                use crate::infra::api::CachePolicy;
                let policy = if offline {
                    CachePolicy::Offline
                } else if refresh {
                    CachePolicy::Refresh
                } else {
                    CachePolicy::PreferFresh
                };
                search::handle_search(config, query, limit, self.verbose, kind, policy).await?;
            }
            Commands::Info {
                package,
                analytics,
                deps,
                kind,
            } => {
                crate::commands::info::info(
                    &config,
                    prefix,
                    &package,
                    kind.formula,
                    kind.cask,
                    deps,
                    analytics,
                )
                .await?;
            }
            Commands::List {
                outdated,
                greedy,
                sizes,
                dates,
                kind,
            } => {
                reject_unsupported(&[(sizes, "--sizes"), (dates, "--dates")])?;
                if greedy && !outdated {
                    bail!("--greedy requires --outdated; no changes were made.");
                }
                if greedy && kind.formula {
                    bail!("--greedy is not supported with --formula; no changes were made.");
                }
                if outdated {
                    native::check_all_updates(&config, prefix, kind.formula, kind.cask, greedy)
                        .await?;
                } else {
                    native::list_all(&config, prefix, kind.formula, kind.cask)?;
                }
            }
            Commands::CheckUpdate { greedy, kind } => {
                if greedy && kind.formula {
                    bail!("--greedy is not supported with --formula; no changes were made.");
                }
                if native::check_all_updates(&config, prefix, kind.formula, kind.cask, greedy)
                    .await?
                {
                    return Ok(ExitCode::from(100));
                }
            }
            Commands::Upgrade {
                packages,
                greedy,
                kind,
                transaction,
            } => {
                if greedy && kind.formula {
                    bail!("--greedy is not supported with --formula; no changes were made.");
                }
                native::upgrade_all(
                    &config,
                    prefix,
                    &packages,
                    kind.formula,
                    kind.cask,
                    greedy,
                    transaction.dry_run,
                    transaction.yes,
                )
                .await?;
            }
            Commands::Update => handlers::refresh_catalogs(&config).await?,
            Commands::Clean { all, dry_run } => {
                handlers::handle_clean(config, all, dry_run).await?;
            }
            Commands::Deps {
                package,
                tree,
                reverse,
            } => {
                crate::commands::deps::deps(&config, prefix, &package, tree, reverse).await?;
            }
            Commands::Analytics {
                package,
                period,
                top,
            } => {
                if let Some(package) = package {
                    crate::commands::info::analytics(&config, &package, &period).await?;
                } else {
                    handlers::handle_analytics(config, period, top).await?;
                }
            }
            Commands::Config { action } => {
                handlers::handle_config(config, action).await?;
            }
        }
        Ok(ExitCode::SUCCESS)
    }
}

fn reject_unsupported(flags: &[(bool, &str)]) -> Result<()> {
    if let Some((_, flag)) = flags.iter().find(|(enabled, _)| *enabled) {
        bail!("{flag} is not supported by the native backend; no changes were made.");
    }
    Ok(())
}
