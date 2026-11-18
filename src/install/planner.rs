use anyhow::Result;

use crate::config::Config;
use super::dependencies::DependencyResolver;
use super::types::{InstallPlan, InstallOptions};

pub struct InstallPlanner {
    resolver: DependencyResolver,
}

impl InstallPlanner {
    pub fn new(config: Config) -> Result<Self> {
        let resolver = DependencyResolver::new(&config)?;
        Ok(Self { resolver })
    }

    /// Create a comprehensive installation plan
    pub async fn create_install_plan(
        &self,
        packages: &[String],
        options: &InstallOptions,
    ) -> Result<InstallPlan> {
        self.resolver.resolve_dependencies(packages, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn test_planner_creation() {
        let config = Config::default();
        let planner = InstallPlanner::new(config);
        assert!(planner.is_ok());
    }
}