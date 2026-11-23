mod handlers;
mod search;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, ConfigEditor};

#[derive(Parser)]
#[command(
    name = "brewrs",
    version,
    about = "A native macOS package manager using Homebrew repositories"
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

    /// Absolute path to an isolated brewrs-owned package prefix
    #[arg(long, global = true)]
    prefix: Option<PathBuf>,
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

#[derive(Subcommand)]
enum Commands {
    /// Alternate spelling for native package commands
    #[command(hide = true)]
    Native {
        #[command(subcommand)]
        action: NativeAction,
    },

    /// Audit a Homebrew installation without changing it
    Migrate {
        /// Absolute Homebrew prefix to inspect (otherwise use HOMEBREW_PREFIX or the platform default)
        #[arg(long)]
        brew_prefix: Option<PathBuf>,
        /// Query official metadata for formula and dependency compatibility
        #[arg(long)]
        check: bool,
        /// Inspect these launchd directories instead of the standard locations
        #[arg(long = "service-dir", value_name = "DIR")]
        service_dirs: Vec<PathBuf>,
    },

    /// Preview or list services for brewrs-managed packages without starting them
    Services {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Register, inspect, plan, or install an explicit local third-party tap
    Tap {
        #[command(subcommand)]
        action: TapAction,
    },

    /// Remove a third-party tap that has no installed packages
    Untap {
        /// Fully qualified tap, for example pproenca/tap
        tap: String,
    },

    /// Print shell environment exports for this brewrs prefix
    Shellenv {
        /// Shell syntax to emit (defaults to $SHELL)
        #[arg(long, value_parser = ["sh", "bash", "zsh", "fish"])]
        shell: Option<String>,
    },

    /// Resolve a formula plan from Homebrew's repository without invoking brew
    Plan {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Override the Homebrew bottle platform tag (e.g. arm64_sequoia)
        #[arg(long)]
        target: Option<String>,
    },

    /// Download and verify formula bottles without installing them
    Fetch {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Override the Homebrew bottle platform tag (e.g. arm64_sequoia)
        #[arg(long)]
        target: Option<String>,
    },

    /// Install formula bottles or supported casks into the brewrs prefix
    #[command(alias = "in")]
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Target directory for installed cask .app bundles (defaults to <prefix>/Applications)
        #[arg(long, conflicts_with = "formula")]
        appdir: Option<PathBuf>,
        /// Force replacement (unsupported for portable bottle installs)
        #[arg(short, long)]
        force: bool,
        /// Skip formula dependencies (unsupported)
        #[arg(long, conflicts_with = "cask")]
        no_deps: bool,
        /// Install from HEAD (unsupported; only stable bottles are supported)
        #[arg(long, conflicts_with = "cask")]
        head: bool,
        #[command(flatten)]
        kind: PackageKind,
        #[command(flatten)]
        transaction: Transaction,
    },

    /// Remove packages from the brewrs prefix
    #[command(alias = "rm")]
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Also remove newly unneeded dependencies
        #[arg(long)]
        autoremove: bool,
        /// Skip confirmation (alias for --assumeyes)
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

    /// List packages in the brewrs prefix
    #[command(alias = "ls")]
    List {
        /// Show available updates instead of the installed inventory
        #[arg(long, conflicts_with_all = ["sizes", "dates"])]
        outdated: bool,
        /// Include auto-updating casks when checking outdated packages
        #[arg(short, long)]
        greedy: bool,
        /// Show disk usage (unsupported)
        #[arg(long)]
        sizes: bool,
        /// Show recorded install dates (unsupported)
        #[arg(long)]
        dates: bool,
        #[command(flatten)]
        kind: PackageKind,
    },

    /// List available updates (exit 100 when updates are found)
    #[command(visible_alias = "check-upgrade")]
    CheckUpdate {
        /// Include auto-updating casks
        #[arg(short, long)]
        greedy: bool,
        #[command(flatten)]
        kind: PackageKind,
    },

    /// Upgrade installed packages
    #[command(alias = "up")]
    Upgrade {
        packages: Vec<String>,
        /// Include auto-updating casks
        #[arg(short, long)]
        greedy: bool,
        #[command(flatten)]
        kind: PackageKind,
        #[command(flatten)]
        transaction: Transaction,
    },

    /// Refresh the brewrs formula and cask catalogs
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
        /// Show brewrs-managed formulae that depend on this package
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
enum NativeAction {
    /// Install supported portable formula bottles without running brew
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        #[command(flatten)]
        transaction: Transaction,
    },
    /// Upgrade selected brewrs-managed formulae (all if omitted)
    Upgrade {
        packages: Vec<String>,
        #[command(flatten)]
        transaction: Transaction,
    },
    /// Remove formulae installed by the native backend
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Also remove newly unneeded dependencies
        #[arg(long)]
        autoremove: bool,
        #[command(flatten)]
        transaction: Transaction,
    },
    /// Remove unused dependencies from the brewrs prefix
    Autoremove {
        #[command(flatten)]
        transaction: Transaction,
    },
    /// List formulae installed by the native backend
    List,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Preview an installed formula's declarative service without starting it
    Plan { package: String },
    /// List brewrs-owned service state without inspecting launchd or processes
    List,
}

#[derive(Subcommand)]
enum TapAction {
    /// Register an explicit local TOML manifest for a fully qualified tap
    Add {
        /// Fully qualified tap, for example pproenca/tap
        tap: String,
        /// Local declarative manifest; it is copied and validated as data
        #[arg(long)]
        manifest: PathBuf,
    },
    /// List registered tap packages without consulting Homebrew
    List,
    /// Preview one fully qualified <owner>/<repo>/<token> tap package
    Plan { package: String },
    /// Install one fully qualified tap package from its pinned artifact
    Install {
        package: String,
        #[command(flatten)]
        transaction: Transaction,
    },
    /// Remove one installed tap package
    Uninstall {
        package: String,
        #[command(flatten)]
        transaction: Transaction,
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
            Commands::Config {
                action: ConfigAction::Edit,
            } => ConfigLoader::load(),
            _ => ConfigLoader::load_read_only(),
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
        let prefix = self.prefix.as_deref();
        use crate::native::install as native;

        match self.command {
            Commands::Native { action } => match action {
                NativeAction::Install {
                    packages,
                    transaction,
                } => {
                    native::install(
                        &config,
                        prefix,
                        &packages,
                        transaction.dry_run,
                        transaction.yes,
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
                crate::native::migration::audit(
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
                    crate::native::services::preview(&config, prefix, &package).await?;
                }
                ServiceAction::List => {
                    crate::native::services::list(&config, prefix)?;
                }
            },
            Commands::Tap { action } => match action {
                TapAction::Add { tap, manifest } => {
                    crate::native::tap::add(&config, prefix, &tap, &manifest)?;
                }
                TapAction::List => crate::native::tap::list(&config, prefix)?,
                TapAction::Plan { package } => {
                    crate::native::tap::plan(&config, prefix, &package)?;
                }
                TapAction::Install {
                    package,
                    transaction,
                } => {
                    crate::native::tap::install(
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
                    crate::native::tap::uninstall(
                        &config,
                        prefix,
                        &package,
                        transaction.dry_run,
                        transaction.yes,
                    )?;
                }
            },
            Commands::Untap { tap } => {
                crate::native::tap::untap(&config, prefix, &tap)?;
            }
            Commands::Shellenv { shell } => {
                native::shellenv(prefix, shell.as_deref())?;
            }
            Commands::Plan { packages, target } => {
                crate::native::run(&config, &packages, target.as_deref(), false).await?;
            }
            Commands::Fetch { packages, target } => {
                crate::native::run(&config, &packages, target.as_deref(), true).await?;
            }
            Commands::Install {
                packages,
                appdir,
                force,
                no_deps,
                head,
                kind,
                transaction,
            } => {
                reject_unsupported(&[
                    (head, "--head"),
                    (no_deps, "--no-deps"),
                    (force, "--force"),
                ])?;
                if kind.cask {
                    crate::native::cask_install::install(
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
                    crate::native::cask_install::remove(
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
                crate::native::info::info(
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
                crate::native::deps::deps(&config, prefix, &package, tree, reverse).await?;
            }
            Commands::Analytics {
                package,
                period,
                top,
            } => {
                if let Some(package) = package {
                    crate::native::info::analytics(&config, &package, &period).await?;
                } else {
                    handlers::handle_analytics(config, period, top).await?;
                }
            }
            Commands::Config { .. } => unreachable!(),
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
