use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use super::types::Config;
use super::validator::ConfigValidator;

pub struct ConfigLoader;

impl ConfigLoader {
    /// Load configuration from file, create default if it doesn't exist
    pub fn load() -> Result<Config> {
        let config_path = Self::config_path()?;

        if config_path.exists() {
            let content = fs::read_to_string(&config_path)
                .with_context(|| format!("Failed to read config file: {:?}", config_path))?;

            let config: Config =
                toml::from_str(&content).with_context(|| "Failed to parse configuration file")?;

            ConfigValidator::validate(&config)?;
            Ok(config)
        } else {
            // Create default config
            let config = Config::default();
            Self::save(&config)?;
            Ok(config)
        }
    }

    /// Save configuration to file
    pub fn save(config: &Config) -> Result<()> {
        let config_path = Self::config_path()?;

        // Create config directory if it doesn't exist
        if let Some(parent) = config_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {:?}", parent))?;
        }

        let content =
            toml::to_string_pretty(config).with_context(|| "Failed to serialize configuration")?;

        fs::write(&config_path, content)
            .with_context(|| format!("Failed to write config file: {:?}", config_path))?;

        Ok(())
    }

    /// Get the configuration file path
    pub fn config_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("BREW_RS_CONFIG").filter(|path| !path.is_empty()) {
            return Ok(PathBuf::from(path));
        }
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Unable to determine config directory"))?;

        Ok(config_dir.join("brew-rs").join("config.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_default_config() {
        let config = Config::default();
        assert_eq!(config.downloads.parallel_downloads, 5);
        assert!(config.general.parallel);
        assert_eq!(config.ui.style, "dnf");
    }
}
