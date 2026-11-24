use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

use super::types::Config;
use super::validator::ConfigValidator;

pub struct ConfigLoader;

impl ConfigLoader {
    /// Load configuration from file, creating defaults when the file is missing.
    pub fn load() -> Result<Config> {
        Self::load_inner(true)
    }

    /// Load configuration without writing defaults on first use.
    pub fn load_read_only() -> Result<Config> {
        Self::load_inner(false)
    }

    fn load_inner(persist_defaults: bool) -> Result<Config> {
        let path = Self::config_path()?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                if persist_defaults {
                    Self::save(&config)?;
                }
                return Ok(config);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read config file: {path:?}"));
            }
        };
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("Failed to parse configuration file: {path:?}"))?;
        ConfigValidator::validate(&config)?;
        Ok(config)
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
mod tests;
