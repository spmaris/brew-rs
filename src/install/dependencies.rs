use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

use crate::api::{BrewApiClient, Formula};
use crate::cache::CacheManager;
use crate::config::Config;
use super::database::InstalledDatabase;
use super::types::{InstallPlan, InstallOptions};

pub struct DependencyResolver {
    client: BrewApiClient,
    cache: CacheManager,
    database: InstalledDatabase,
}

impl DependencyResolver {
    pub fn new(config: &Config) -> Result<Self> {
        let client = BrewApiClient::new(config)?;
        let cache = CacheManager::new(config)?;
        let database = InstalledDatabase::new(config)?;

        Ok(Self {
            client,
            cache,
            database,
        })
    }

    /// Resolve dependencies for a list of packages and create an install plan
    pub async fn resolve_dependencies(
        &self,
        packages: &[String],
        options: &InstallOptions,
    ) -> Result<InstallPlan> {
        let mut plan = InstallPlan::new();
        let mut to_install = HashMap::new();
        let mut to_process: Vec<String> = packages.iter().cloned().collect();
        let mut processed = HashSet::new();

        while let Some(package_name) = to_process.pop() {
            if processed.contains(&package_name) || to_install.contains_key(&package_name) {
                continue;
            }
            processed.insert(package_name.clone());

            if self.database.is_installed(&package_name).await? && !options.force {
                plan.add_warning(format!("Package '{}' is already installed. Use --force to reinstall.", package_name));
                continue;
            }
            
            let formula = match self.get_formula(&package_name).await {
                Ok(f) => f,
                Err(e) => {
                    plan.add_conflict(format!("Package '{}' not found: {}", package_name, e));
                    continue;
                }
            };
            
            to_install.insert(package_name, formula.clone());

            if !options.no_deps {
                for dep in &formula.dependencies {
                    if !self.database.is_installed(dep).await? {
                        to_process.push(dep.clone());
                    }
                }
            }
        }
        
        for package_name in packages {
            if let Some(formula) = to_install.remove(package_name) {
                plan.to_install.push(formula);
            }
        }
        
        plan.dependencies = to_install.into_values().collect();
        self.check_conflicts(&mut plan, &options).await?;

        Ok(plan)
    }

    /// Check for conflicts between packages in the plan
    async fn check_conflicts(&self, plan: &mut InstallPlan, options: &InstallOptions) -> Result<()> {
        let all_packages: Vec<Formula> = plan.to_install.iter()
            .chain(plan.dependencies.iter())
            .cloned()
            .collect();

        for formula in &all_packages {
            if self.database.is_installed(&formula.name).await? && !options.force {
                plan.add_warning(format!(
                    "Package '{}' is already installed. Use --force to reinstall.",
                    formula.name
                ));
            }

            if formula.disabled {
                plan.add_conflict(format!(
                    "Package '{}' is disabled and cannot be installed.",
                    formula.name
                ));
            }
        }

        Ok(())
    }

    /// Get formula from cache or API
    async fn get_formula(&self, name: &str) -> Result<Formula> {
        if let Some(formula) = self.cache.get_formula(name).await {
            return Ok(formula);
        }

        let formula = self.client.get_formula(name).await
            .with_context(|| format!("Package '{}' not found", name))?;
        
        self.cache.cache_formula_default(name.to_string(), formula.clone()).await;
        
        Ok(formula)
    }
}