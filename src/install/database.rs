use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tokio::sync::RwLock;

use crate::config::Config;
use super::types::InstalledPackage;

pub struct InstalledDatabase {
    db_path: PathBuf,
    packages: RwLock<HashMap<String, InstalledPackage>>,
}

impl InstalledDatabase {
    pub fn new(config: &Config) -> Result<Self> {
        let db_path = &config.storage.db_path;
        
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let packages = if db_path.exists() {
            let content = fs::read_to_string(db_path)?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        };

        Ok(Self {
            db_path: db_path.clone(),
            packages: RwLock::new(packages),
        })
    }

    /// Check if a package is installed
    pub async fn is_installed(&self, name: &str) -> Result<bool> {
        let packages = self.packages.read().await;
        Ok(packages.contains_key(name))
    }

    /// List all installed packages
    pub async fn list_installed(&self) -> Vec<InstalledPackage> {
        let packages = self.packages.read().await;
        packages.values().cloned().collect()
    }

    /// Get packages that depend on the given package
    pub async fn get_dependents(&self, name: &str) -> Vec<String> {
        let packages = self.packages.read().await;
        packages
            .values()
            .filter(|pkg| pkg.dependencies.contains(&name.to_string()))
            .map(|pkg| pkg.name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use crate::api::types::Formula;

    fn create_test_db(temp_dir: &tempfile::TempDir) -> (Config, InstalledDatabase) {
        let mut config = Config::default();
        config.storage.db_path = temp_dir.path().join("test.json");
        let db = InstalledDatabase::new(&config).unwrap();
        (config, db)
    }

    #[tokio::test]
    async fn test_db_creation() -> Result<()> {
        let temp_dir = tempdir()?;
        let _ = create_test_db(&temp_dir);
        assert!(temp_dir.path().join("test.json").exists());
        Ok(())
    }
    
    // A test helper to add a package to the db for other tests.
    async fn add_package(db: &InstalledDatabase, formula: &Formula) {
        let mut packages = db.packages.write().await;
        let pkg = InstalledPackage {
            name: formula.name.clone(),
            version: formula.versions.stable.clone(),
            dependencies: formula.dependencies.clone(),
            ..Default::default()
        };
        packages.insert(formula.name.clone(), pkg);
    }

    #[tokio::test]
    async fn test_is_installed() -> Result<()> {
        let temp_dir = tempdir()?;
        let (_, db) = create_test_db(&temp_dir);
        let formula = Formula { name: "test-package".to_string(), ..Default::default() };

        assert!(!db.is_installed("test-package").await?);
        add_package(&db, &formula).await;
        assert!(db.is_installed("test-package").await?);
        Ok(())
    }
}