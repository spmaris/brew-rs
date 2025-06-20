use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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
                cache_retention: 30,   // 30 days
                db_path: home_dir.join(".local/share/brew-rs/db.sqlite"),
            },
        }
    }
}

impl Config {
    /// Load configuration from file, create default if it doesn't exist
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;
        
        if config_path.exists() {
            let content = fs::read_to_string(&config_path)
                .with_context(|| format!("Failed to read config file: {:?}", config_path))?;
            
            let config: Config = toml::from_str(&content)
                .with_context(|| "Failed to parse configuration file")?;
            
            config.validate()?;
            Ok(config)
        } else {
            // Create default config
            let config = Config::default();
            config.save()?;
            Ok(config)
        }
    }

    /// Save configuration to file
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;
        
        // Create config directory if it doesn't exist
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {:?}", parent))?;
        }

        let content = toml::to_string_pretty(self)
            .with_context(|| "Failed to serialize configuration")?;
        
        fs::write(&config_path, content)
            .with_context(|| format!("Failed to write config file: {:?}", config_path))?;
        
        Ok(())
    }

    /// Get the configuration file path
    fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Unable to determine config directory"))?;
        
        Ok(config_dir.join("brew-rs").join("config.toml"))
    }

    /// Validate configuration values
    fn validate(&self) -> Result<()> {
        // Validate parallel downloads count
        if self.downloads.parallel_downloads == 0 {
            anyhow::bail!("parallel_downloads must be at least 1");
        }
        if self.downloads.parallel_downloads > 50 {
            anyhow::bail!("parallel_downloads cannot exceed 50");
        }

        // Validate timeout
        if self.downloads.timeout == 0 {
            anyhow::bail!("timeout must be greater than 0");
        }

        // Validate UI style
        let valid_styles = ["dnf", "apt", "pacman", "minimal"];
        if !valid_styles.contains(&self.ui.style.as_str()) {
            anyhow::bail!("Invalid UI style. Valid options: {}", valid_styles.join(", "));
        }

        // Validate prefix exists and is writable
        if !self.install.prefix.exists() {
            anyhow::bail!("Installation prefix does not exist: {:?}", self.install.prefix);
        }

        Ok(())
    }

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

// Configuration editing utilities
impl Config {
    /// Set parallel download count with validation
    pub fn set_parallel_downloads(&mut self, count: u8) -> Result<()> {
        if count == 0 || count > 50 {
            anyhow::bail!("Parallel downloads must be between 1 and 50");
        }
        self.downloads.parallel_downloads = count;
        Ok(())
    }

    /// Set UI style with validation
    pub fn set_ui_style(&mut self, style: &str) -> Result<()> {
        let valid_styles = ["dnf", "apt", "pacman", "minimal"];
        if !valid_styles.contains(&style) {
            anyhow::bail!("Invalid UI style. Valid options: {}", valid_styles.join(", "));
        }
        self.ui.style = style.to_string();
        Ok(())
    }

    /// Toggle parallel operations
    pub fn toggle_parallel(&mut self) {
        self.general.parallel = !self.general.parallel;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.downloads.parallel_downloads, 5);
        assert!(config.general.parallel);
        assert_eq!(config.ui.style, "dnf");
    }

    #[test]
    fn test_validation() {
        let mut config = Config::default();
        
        // Test invalid parallel downloads
        config.downloads.parallel_downloads = 0;
        assert!(config.validate().is_err());
        
        config.downloads.parallel_downloads = 100;
        assert!(config.validate().is_err());
        
        // Test invalid style
        config.downloads.parallel_downloads = 5;
        config.ui.style = "invalid".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_parallel_downloads_calculation() {
        let mut config = Config::default();
        
        // Parallel enabled
        config.general.parallel = true;
        config.downloads.parallel_downloads = 10;
        assert_eq!(config.parallel_downloads(), 10);
        
        // Parallel disabled
        config.general.parallel = false;
        assert_eq!(config.parallel_downloads(), 1);
    }
}