use anyhow::Result;
use super::types::Config;

pub struct ConfigValidator;

impl ConfigValidator {
    /// Validate configuration values
    pub fn validate(config: &Config) -> Result<()> {
        Self::validate_downloads(&config.downloads)?;
        Self::validate_ui(&config.ui)?;
        Self::validate_install(&config.install)?;
        Self::validate_storage(&config.storage)?;
        Ok(())
    }

    fn validate_downloads(downloads: &super::types::DownloadConfig) -> Result<()> {
        if downloads.parallel_downloads == 0 {
            anyhow::bail!("parallel_downloads must be at least 1");
        }
        if downloads.parallel_downloads > 50 {
            anyhow::bail!("parallel_downloads cannot exceed 50");
        }

        if downloads.timeout == 0 {
            anyhow::bail!("timeout must be greater than 0");
        }

        if downloads.retries > 10 {
            anyhow::bail!("retries cannot exceed 10");
        }

        Ok(())
    }

    fn validate_ui(ui: &super::types::UiConfig) -> Result<()> {
        let valid_styles = ["dnf", "apt", "pacman", "minimal"];
        if !valid_styles.contains(&ui.style.as_str()) {
            anyhow::bail!("Invalid UI style. Valid options: {}", valid_styles.join(", "));
        }

        Ok(())
    }

    fn validate_install(install: &super::types::InstallConfig) -> Result<()> {
        if install.prefix.exists() {
            let metadata = std::fs::metadata(&install.prefix)
                .map_err(|e| anyhow::anyhow!("Cannot access installation prefix {:?}: {}", install.prefix, e))?;
            
            if !metadata.is_dir() {
                anyhow::bail!("Installation prefix is not a directory: {:?}", install.prefix);
            }
        }

        Ok(())
    }

    fn validate_storage(storage: &super::types::StorageConfig) -> Result<()> {
        if let Some(parent) = storage.cache_dir.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("Cannot create cache directory parent {:?}: {}", parent, e))?;
            }
        }

        if let Some(parent) = storage.db_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("Cannot create database directory parent {:?}: {}", parent, e))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_config() {
        let config = Config::default();
        assert!(ConfigValidator::validate(&config).is_ok());
    }

    #[test]
    fn test_invalid_parallel_downloads() {
        let mut config = Config::default();
        config.downloads.parallel_downloads = 0;
        assert!(ConfigValidator::validate(&config).is_err());
        
        config.downloads.parallel_downloads = 100;
        assert!(ConfigValidator::validate(&config).is_err());
    }

    #[test]
    fn test_invalid_ui_style() {
        let mut config = Config::default();
        config.ui.style = "invalid".to_string();
        assert!(ConfigValidator::validate(&config).is_err());
    }
}