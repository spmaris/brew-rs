pub mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};
use crate::config::Config;

#[derive(Parser)]
#[command(
    name = "brew-rs",
    about = "A fast, modern package manager compatible with Homebrew formulae",
    version = env!("CARGO_PKG_VERSION"),
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
    
    /// Enable verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,
    
    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,
    
    /// Override parallel downloads setting
    #[arg(long, global = true)]
    pub parallel: Option<u8>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Install packages
    #[command(alias = "in")]
    Install {
        /// Package names to install
        packages: Vec<String>,
        
        /// Force installation even if already installed
        #[arg(short, long)]
        force: bool,
        
        /// Don't install dependencies
        #[arg(long)]
        no_deps: bool,
        
        /// Install from HEAD instead of stable
        #[arg(long)]
        head: bool,
    },
    
    /// Remove packages
    #[command(alias = "rm")]
    Remove {
        /// Package names to remove
        packages: Vec<String>,
        
        /// Remove dependencies that are no longer needed
        #[arg(long)]
        autoremove: bool,
        
        /// Force removal without confirmation
        #[arg(short, long)]
        force: bool,
    },
    
    /// Search for packages
    #[command(alias = "se")]
    Search {
        /// Search query
        query: String,
        
        /// Limit number of results
        #[arg(short, long, default_value = "20")]
        limit: usize,
        
        /// Show detailed information
        #[arg(short, long)]
        verbose: bool,
    },
    
    /// Show package information
    #[command(alias = "show")]
    Info {
        /// Package name
        package: String,
        
        /// Show installation analytics
        #[arg(long)]
        analytics: bool,
        
        /// Show dependency tree
        #[arg(long)]
        deps: bool,
    },
    
    /// List installed packages
    #[command(alias = "ls")]
    List {
        /// Show only outdated packages
        #[arg(long)]
        outdated: bool,
        
        /// Show package sizes
        #[arg(long)]
        sizes: bool,
        
        /// Show installation dates
        #[arg(long)]
        dates: bool,
    },
    
    /// Update package database and upgrade packages
    #[command(alias = "up")]
    Upgrade {
        /// Specific packages to upgrade
        packages: Vec<String>,
        
        /// Show what would be upgraded without doing it
        #[arg(long)]
        dry_run: bool,
    },
    
    /// Clean cache and temporary files
    Clean {
        /// Clean everything including downloaded bottles
        #[arg(long)]
        all: bool,
        
        /// Show what would be cleaned without doing it
        #[arg(long)]
        dry_run: bool,
    },
    
    /// Show and modify configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    
    /// Show dependency tree for packages
    Deps {
        /// Package name
        package: String,
        
        /// Show as tree format
        #[arg(long)]
        tree: bool,
        
        /// Include reverse dependencies
        #[arg(long)]
        reverse: bool,
    },
    
    /// Show analytics and popularity data
    Analytics {
        /// Package name (optional)
        package: Option<String>,
        
        /// Time period: 30d, 90d, 365d
        #[arg(short, long, default_value = "30d")]
        period: String,
        
        /// Show top N packages
        #[arg(short, long, default_value = "20")]
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
    Set {
        /// Configuration key (e.g., downloads.parallel_downloads)
        key: String,
        /// Configuration value
        value: String,
    },
    
    /// Reset configuration to defaults
    Reset,
}

impl Cli {
    pub async fn execute(self, mut config: Config) -> Result<()> {
        // Apply global overrides
        if self.no_color {
            config.ui.colored = false;
        }
        
        if let Some(parallel) = self.parallel {
            config.set_parallel_downloads(parallel)?;
        }
        
        // Execute the command
        match self.command {
            Commands::Install { packages, force, no_deps, head } => {
                crate::cli::commands::install(config, packages, force, no_deps, head).await
            }
            Commands::Remove { packages, autoremove, force } => {
                crate::cli::commands::remove(config, packages, autoremove, force).await
            }
            Commands::Search { query, limit, verbose } => {
                crate::cli::commands::search(config, query, limit, verbose).await
            }
            Commands::Info { package, analytics, deps } => {
                crate::cli::commands::info(config, package, analytics, deps).await
            }
            Commands::List { outdated, sizes, dates } => {
                crate::cli::commands::list(config, outdated, sizes, dates).await
            }
            Commands::Upgrade { packages, dry_run } => {
                crate::cli::commands::upgrade(config, packages, dry_run).await
            }
            Commands::Clean { all, dry_run } => {
                crate::cli::commands::clean(config, all, dry_run).await
            }
            Commands::Config { action } => {
                crate::cli::commands::config_cmd(config, action).await
            }
            Commands::Deps { package, tree, reverse } => {
                crate::cli::commands::deps(config, package, tree, reverse).await
            }
            Commands::Analytics { package, period, top } => {
                crate::cli::commands::analytics(config, package, period, top).await
            }
        }
    }
}