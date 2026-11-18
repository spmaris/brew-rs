use anyhow::Result;
use super::types::Config;
use super::loader::ConfigLoader;
use super::validator::ConfigValidator;

pub struct ConfigEditor;

impl ConfigEditor {
    /// Set parallel download count with validation
    pub fn set_parallel_downloads(config: &mut Config, count: u8) -> Result<()> {
        if count == 0 || count > 50 {
            anyhow::bail!("Parallel downloads must be between 1 and 50");
        }
        config.downloads.parallel_downloads = count;
        Ok(())
    }

    /// Set UI style with validation
    pub fn set_ui_style(config: &mut Config, style: &str) -> Result<()> {
        let valid_styles = ["dnf", "apt", "pacman", "minimal"];
        if !valid_styles.contains(&style) {
            anyhow::bail!("Invalid UI style. Valid options: {}", valid_styles.join(", "));
        }
        config.ui.style = style.to_string();
        Ok(())
    }

    /// Set download timeout with validation
    pub fn set_timeout(config: &mut Config, timeout: u64) -> Result<()> {
        if timeout == 0 {
            anyhow::bail!("Timeout must be greater than 0");
        }
        config.downloads.timeout = timeout;
        Ok(())
    }

    /// Set cache size limit with validation
    pub fn set_cache_size(config: &mut Config, size_mb: u64) -> Result<()> {
        config.storage.max_cache_size = size_mb;
        Ok(())
    }

    /// Set cache retention with validation
    pub fn set_cache_retention(config: &mut Config, days: u32) -> Result<()> {
        config.storage.cache_retention = days;
        Ok(())
    }

    /// Set configuration value by key path (e.g., "downloads.parallel_downloads")
    pub fn set_value_by_path(config: &mut Config, key_path: &str, value: &str) -> Result<()> {
        let parts: Vec<&str> = key_path.split('.').collect();
        
        match parts.as_slice() {
            ["downloads", "parallel_downloads"] => {
                let count: u8 = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid number: {}", value))?;
                Self::set_parallel_downloads(config, count)?;
            }
            ["downloads", "timeout"] => {
                let timeout: u64 = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid number: {}", value))?;
                Self::set_timeout(config, timeout)?;
            }
            ["downloads", "retries"] => {
                let retries: u8 = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid number: {}", value))?;
                if retries > 10 {
                    anyhow::bail!("Retries cannot exceed 10");
                }
                config.downloads.retries = retries;
            }
            ["ui", "style"] => {
                Self::set_ui_style(config, value)?;
            }
            ["ui", "colored"] => {
                let colored: bool = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid boolean: {}", value))?;
                config.ui.colored = colored;
            }
            ["ui", "progress_bars"] => {
                let progress_bars: bool = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid boolean: {}", value))?;
                config.ui.progress_bars = progress_bars;
            }
            ["general", "parallel"] => {
                let parallel: bool = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid boolean: {}", value))?;
                config.general.parallel = parallel;
            }
            ["general", "confirm_operations"] => {
                let confirm: bool = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid boolean: {}", value))?;
                config.general.confirm_operations = confirm;
            }
            ["general", "auto_clean"] => {
                let auto_clean: bool = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid boolean: {}", value))?;
                config.general.auto_clean = auto_clean;
            }
            ["storage", "max_cache_size"] => {
                let size: u64 = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid number: {}", value))?;
                Self::set_cache_size(config, size)?;
            }
            ["storage", "cache_retention"] => {
                let retention: u32 = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid number: {}", value))?;
                Self::set_cache_retention(config, retention)?;
            }
            ["install", "verify_checksums"] => {
                let verify: bool = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid boolean: {}", value))?;
                config.install.verify_checksums = verify;
            }
            ["install", "auto_deps"] => {
                let auto_deps: bool = value.parse()
                    .map_err(|_| anyhow::anyhow!("Invalid boolean: {}", value))?;
                config.install.auto_deps = auto_deps;
            }
            _ => {
                anyhow::bail!("Unknown configuration key: {}", key_path);
            }
        }

        // Validate the updated configuration
        ConfigValidator::validate(config)?;
        Ok(())
    }
    
    /// Save configuration after editing
    pub fn save_config(config: &Config) -> Result<()> {
        ConfigValidator::validate(config)?;
        ConfigLoader::save(config)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_parallel_downloads() {
        let mut config = Config::default();
        
        assert!(ConfigEditor::set_parallel_downloads(&mut config, 10).is_ok());
        assert_eq!(config.downloads.parallel_downloads, 10);
        
        assert!(ConfigEditor::set_parallel_downloads(&mut config, 0).is_err());
        assert!(ConfigEditor::set_parallel_downloads(&mut config, 100).is_err());
    }

    #[test]
    fn test_set_ui_style() {
        let mut config = Config::default();
        
        assert!(ConfigEditor::set_ui_style(&mut config, "apt").is_ok());
        assert_eq!(config.ui.style, "apt");
        
        assert!(ConfigEditor::set_ui_style(&mut config, "invalid").is_err());
    }

    #[test]
    fn test_set_value_by_path() {
        let mut config = Config::default();
        
        assert!(ConfigEditor::set_value_by_path(&mut config, "downloads.parallel_downloads", "10").is_ok());
        assert_eq!(config.downloads.parallel_downloads, 10);
        
        assert!(ConfigEditor::set_value_by_path(&mut config, "ui.style", "apt").is_ok());
        assert_eq!(config.ui.style, "apt");
        
        assert!(ConfigEditor::set_value_by_path(&mut config, "invalid.path", "value").is_err());
    }
}