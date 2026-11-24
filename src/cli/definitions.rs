use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "brewrs",
    version,
    about = "A native macOS package manager using Homebrew repositories"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(super) command: Commands,

    /// Show more detail
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Override brew-rs's parallel downloads setting
    #[arg(long, global = true)]
    pub(super) parallel: Option<u8>,

    /// Absolute path to an isolated brewrs-owned package prefix
    #[arg(long, global = true)]
    pub(super) prefix: Option<PathBuf>,
}

#[derive(Args, Clone, Copy, Default)]
pub(super) struct Transaction {
    /// Preview without changing installed packages
    #[arg(short = 'n', long)]
    pub(super) dry_run: bool,

    /// Skip confirmation
    #[arg(short = 'y', long = "assumeyes", visible_alias = "yes")]
    pub(super) yes: bool,
}

#[derive(Args, Clone, Copy, Default)]
pub(super) struct PackageKind {
    /// Treat targets as formulae
    #[arg(long, conflicts_with = "cask")]
    pub(super) formula: bool,

    /// Treat targets as casks
    #[arg(long)]
    pub(super) cask: bool,
}

#[derive(Subcommand)]
pub(super) enum Commands {
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

    /// Preview or list services for installed packages without starting them
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
        /// Package source: `homebrew` (default) or the reviewed official `upstream` build
        #[arg(long, value_enum, conflicts_with = "cask")]
        source: Option<crate::commands::install::SourceFlag>,
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
        /// Show installed formulae that depend on this package
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
pub(super) enum NativeAction {
    /// Install supported portable formula bottles without running brew
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Package source: `homebrew` (default) or the reviewed official `upstream` build
        #[arg(long, value_enum)]
        source: Option<crate::commands::install::SourceFlag>,
        #[command(flatten)]
        transaction: Transaction,
    },
    /// Upgrade selected formulae (all if omitted)
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
pub(super) enum ServiceAction {
    /// Preview an installed formula's declarative service without starting it
    Plan { package: String },
    /// List brewrs-owned service state without inspecting launchd or processes
    List,
}

#[derive(Subcommand)]
pub(super) enum TapAction {
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
pub(super) enum ConfigAction {
    /// Show current configuration
    Show,
    /// Edit configuration file
    Edit,
    /// Set a configuration value
    Set { key: String, value: String },
    /// Reset configuration to defaults
    Reset,
}
