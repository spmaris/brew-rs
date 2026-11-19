use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// General configuration options
    pub general: GeneralConfig,
    /// Download and network settings
    pub downloads: DownloadConfig,
    /// Installation preferences
    pub install: InstallConfig,
    /// UI and output preferences
    pub ui: UiConfig,
    /// Cache and storage settings
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// Enable parallel operations (default: true)
    pub parallel: bool,
    /// Confirm before installing/removing packages (default: true)
    pub confirm_operations: bool,
    /// Automatically clean cache after operations (default: false)
    pub auto_clean: bool,
    /// Check for updates on startup (default: true)
    pub check_updates: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Number of parallel downloads (default: 5, max: 50)
    pub parallel_downloads: u8,
    /// Download timeout in seconds (default: 30)
    pub timeout: u64,
    /// Number of retry attempts (default: 3)
    pub retries: u8,
    /// Use system proxy settings (default: true)
    pub use_proxy: bool,
    /// Custom user agent string
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallConfig {
    /// Keep downloaded bottles after installation (default: false)
    pub keep_bottles: bool,
    /// Verify checksums for all downloads (default: true)
    pub verify_checksums: bool,
    /// Install dependencies automatically (default: true)
    pub auto_deps: bool,
    /// Prefer bottles over building from source (default: true)
    pub prefer_bottles: bool,
    /// Installation prefix (default: /opt/homebrew for ARM Macs)
    pub prefix: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Use colored output (default: true)
    pub colored: bool,
    /// Show progress bars (default: true)
    pub progress_bars: bool,
    /// Output style: "dnf", "apt", "pacman", "minimal" (default: "dnf")
    pub style: String,
    /// Show package download sizes (default: true)
    pub show_sizes: bool,
    /// Show installation time estimates (default: true)
    pub show_eta: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Cache directory for downloaded bottles and metadata
    pub cache_dir: PathBuf,
    /// Maximum cache size in MB (default: 1000, 0 = unlimited)
    pub max_cache_size: u64,
    /// Cache retention in days (default: 30, 0 = forever)
    pub cache_retention: u32,
    /// Database file for tracking installations
    pub db_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        let home_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| home_dir.join(".cache"))
            .join("brew-rs");
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| home_dir.join(".local/share"))
            .join("brew-rs");

        Self {
            general: GeneralConfig {
                parallel: true,
                confirm_operations: true,
                auto_clean: false,
                check_updates: true,
            },
            downloads: DownloadConfig {
                parallel_downloads: 5,
                timeout: 30,
                retries: 3,
                use_proxy: true,
                user_agent: Some("brew-rs/0.1.0".to_string()),
            },
            install: InstallConfig {
                keep_bottles: false,
                verify_checksums: true,
                auto_deps: true,
                prefer_bottles: true,
                prefix: PathBuf::from("/opt/homebrew"),
            },
            ui: UiConfig {
                colored: true,
                progress_bars: true,
                style: "dnf".to_string(),
                show_sizes: true,
                show_eta: true,
            },
            storage: StorageConfig {
                cache_dir,
                max_cache_size: 1000, // 1GB
                cache_retention: 30,  // 30 days
                db_path: data_dir.join("installed.json"),
            },
        }
    }
}

impl Config {
    /// Get the effective number of parallel downloads
    pub fn parallel_downloads(&self) -> usize {
        if self.general.parallel {
            self.downloads.parallel_downloads as usize
        } else {
            1
        }
    }

    /// Check if we should use colored output
    pub fn use_colors(&self) -> bool {
        self.ui.colored && console::colors_enabled()
    }
}
