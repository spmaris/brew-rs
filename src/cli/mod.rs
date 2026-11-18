mod handlers;
mod info;
mod install;
mod list;
mod search;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, ConfigEditor};

#[derive(Parser)]
#[command(
    name = "brewrs",
    version,
    about = "A fast command line for Homebrew, inspired by uv"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Show more detail
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Override brew-rs's parallel downloads setting
    #[arg(long, global = true)]
    parallel: Option<u8>,
}

#[derive(Args, Clone, Copy, Default)]
struct Transaction {
    /// Preview without changing installed packages
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Skip confirmation
    #[arg(short = 'y', long = "assumeyes", visible_alias = "yes")]
    yes: bool,
}

#[derive(Args, Clone, Copy, Default)]
struct PackageKind {
    /// Treat targets as formulae
    #[arg(long, conflicts_with = "cask")]
    formula: bool,

    /// Treat targets as casks
    #[arg(long)]
    cask: bool,
}

impl PackageKind {
    fn flag(self) -> Option<&'static str> {
        if self.formula {
            Some("--formula")
        } else if self.cask {
            Some("--cask")
        } else {
            None
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Install formulae or casks
    #[command(alias = "in")]
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Pass Homebrew's force-install option
        #[arg(short, long)]
        force: bool,
        /// Skip formula dependencies (unsupported by Homebrew for general use)
        #[arg(long, conflicts_with = "cask")]
        no_deps: bool,
        /// Install from HEAD instead of stable
        #[arg(long, conflicts_with = "cask")]
        head: bool,
        #[command(flatten)]
        kind: PackageKind,
        #[command(flatten)]
        transaction: Transaction,
    },

    /// Remove formulae or casks
    #[command(alias = "rm")]
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Also remove newly unneeded dependencies
        #[arg(long)]
        autoremove: bool,
        /// Skip confirmation (alias for --assumeyes; does not force Homebrew)
        #[arg(short, long)]
        force: bool,
        #[command(flatten)]
        kind: PackageKind,
        #[command(flatten)]
        transaction: Transaction,
    },

    /// Remove formulae no longer needed as dependencies
    Autoremove {
        #[command(flatten)]
        transaction: Transaction,
    },

    /// Search for packages by name or description
    #[command(alias = "se")]
    Search {
        query: String,
        /// Maximum total results
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
        /// Use only locally cached catalogs, even if stale
        #[arg(long, conflicts_with = "refresh")]
        offline: bool,
        /// Fetch fresh catalogs instead of using the local cache
        #[arg(long)]
        refresh: bool,
        #[command(flatten)]
        kind: PackageKind,
    },

    /// Show package information
    #[command(alias = "show")]
    Info {
        package: String,
        /// Show installation analytics
        #[arg(long)]
        analytics: bool,
        /// Show direct dependencies
        #[arg(long)]
        deps: bool,
        #[command(flatten)]
        kind: PackageKind,
    },

    /// List installed formulae and casks
    #[command(alias = "ls")]
    List {
        /// Show available updates instead of the installed inventory
        #[arg(long, conflicts_with_all = ["sizes", "dates"])]
        outdated: bool,
        /// Include Homebrew's disk usage report
        #[arg(long)]
        sizes: bool,
        /// Show recorded install dates (UTC)
        #[arg(long)]
        dates: bool,
    },

    /// List available updates (exit 100 when updates are found)
    #[command(visible_alias = "check-upgrade")]
    CheckUpdate {
        /// Include auto-updating casks and casks with no fixed version
        #[arg(short, long)]
        greedy: bool,
    },

    /// Upgrade installed packages
    #[command(alias = "up")]
    Upgrade {
        packages: Vec<String>,
        /// Include auto-updating casks and casks with no fixed version
        #[arg(short, long)]
        greedy: bool,
        #[command(flatten)]
        kind: PackageKind,
        #[command(flatten)]
        transaction: Transaction,
    },

    /// Refresh Homebrew metadata without upgrading packages
    Update,

    /// Clean brew-rs cache and temporary files
    Clean {
        /// Clean everything including downloaded bottles
        #[arg(long)]
        all: bool,
        /// Show the cleanup scope without deleting anything
        #[arg(short = 'n', long)]
        dry_run: bool,
    },

    /// Show and modify configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Show formula dependencies or installed dependents
    Deps {
        package: String,
        /// Show a dependency tree (recursive dependents with --reverse)
        #[arg(long)]
        tree: bool,
        /// Show installed formulae and casks that depend on this package
        #[arg(long)]
        reverse: bool,
    },

    /// Show analytics and popularity data
    Analytics {
        package: Option<String>,
        /// Time period
        #[arg(short, long, default_value = "30d", value_parser = ["30d", "90d", "365d"])]
        period: String,
        /// Maximum results (global analytics only)
        #[arg(short, long, default_value_t = 20)]
        top: usize,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Show current configuration
    Show,
    /// Edit configuration file
    Edit,
    /// Set a configuration value
    Set { key: String, value: String },
    /// Reset configuration to defaults
    Reset,
}

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
            _ => ConfigLoader::load(),
        }
    }

    pub async fn execute(self, mut config: Config) -> Result<ExitCode> {
        if let Commands::Config { action } = self.command {
            // Display overrides must not be saved to the config file.
            handlers::handle_config(config, action).await?;
            return Ok(ExitCode::SUCCESS);
        }

        if self.no_color {
            config.ui.colored = false;
        }
        if let Some(parallel) = self.parallel {
            ConfigEditor::set_parallel_downloads(&mut config, parallel)?;
        }
        let brew = crate::brew::Brew::new(&config, self.verbose);

        match self.command {
            Commands::Install {
                packages,
                force,
                no_deps,
                head,
                kind,
                transaction,
            } => {
                let mut options = Vec::new();
                if force {
                    options.push("--force");
                }
                if no_deps {
                    options.push("--ignore-dependencies");
                }
                if head {
                    options.push("--HEAD");
                }
                let selector = kind
                    .flag()
                    .or_else(|| (no_deps || head).then_some("--formula"));
                options.extend(selector);
                install::install_or_upgrade(
                    &config,
                    brew,
                    "install",
                    options,
                    &packages,
                    transaction,
                )
                .await?;
            }
            Commands::Remove {
                packages,
                autoremove,
                force,
                kind,
                mut transaction,
            } => {
                transaction.yes |= force;
                install::remove(&config, brew, &packages, kind, transaction, autoremove).await?;
            }
            Commands::Autoremove { transaction } => {
                install::autoremove(&config, brew, transaction).await?;
            }
            Commands::Search {
                query,
                limit,
                kind,
                offline,
                refresh,
            } => {
                use crate::api::CachePolicy;
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
                info::handle_info(&config, brew, &package, analytics, deps, kind).await?;
            }
            Commands::List { outdated: true, .. } => {
                list::show_outdated(&config, brew, false).await?;
            }
            Commands::List { sizes, dates, .. } => {
                list::handle_list(&config, brew, sizes, dates).await?;
            }
            Commands::CheckUpdate { greedy } => {
                if list::show_outdated(&config, brew, greedy).await? {
                    return Ok(ExitCode::from(100));
                }
            }
            Commands::Upgrade {
                packages,
                greedy,
                kind,
                transaction,
            } => {
                let mut options: Vec<_> = kind.flag().into_iter().collect();
                if greedy {
                    options.push("--greedy");
                }
                install::install_or_upgrade(
                    &config,
                    brew,
                    "upgrade",
                    options,
                    &packages,
                    transaction,
                )
                .await?;
            }
            Commands::Update => {
                brew.run("update", &[], false).await?;
            }
            Commands::Clean { all, dry_run } => {
                handlers::handle_clean(config, all, dry_run).await?;
            }
            Commands::Deps {
                package,
                tree,
                reverse,
            } => {
                handlers::handle_deps(brew, &package, tree, reverse).await?;
            }
            Commands::Analytics {
                package,
                period,
                top,
            } => {
                handlers::handle_analytics(config, brew, package, period, top).await?;
            }
            Commands::Config { .. } => unreachable!(),
        }
        Ok(ExitCode::SUCCESS)
    }
}
