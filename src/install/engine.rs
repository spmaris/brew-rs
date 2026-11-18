use anyhow::Result;

use crate::config::Config;
use crate::ui::DnfStyleFormatter;
use super::planner::InstallPlanner;
use super::types::{InstallOptions, InstallPlan};

pub struct InstallationEngine {
    config: Config,
    formatter: DnfStyleFormatter,
    planner: InstallPlanner,
}

impl InstallationEngine {
    pub fn new(config: Config) -> Result<Self> {
        let formatter = DnfStyleFormatter::new(&config);
        let planner = InstallPlanner::new(config.clone())?;

        Ok(Self {
            config,
            formatter,
            planner,
        })
    }

    /// Create an installation plan and display it.
    pub async fn plan_and_display(&self, packages: &[String], options: &InstallOptions) -> Result<()> {
        let plan = self.planner.create_install_plan(packages, options).await?;

        if !plan.is_valid() {
            println!("\n{}", self.formatter.error("Cannot proceed due to conflicts:"));
            for conflict in &plan.conflicts {
                println!("  - {}", self.formatter.error(conflict));
            }
            return Ok(());
        }

        self.show_transaction_summary(&plan);
        Ok(())
    }

    fn show_transaction_summary(&self, plan: &InstallPlan) {
        println!("{}", self.formatter.header("Transaction Summary"));

        if !plan.dependencies.is_empty() {
            println!("\n{}", self.formatter.section("Dependencies to be installed:"));
            for dep in &plan.dependencies {
                println!("  {} {} {}", 
                    self.formatter.bullet_available(),
                    self.formatter.package_name(&dep.name, false),
                    self.formatter.package_version(dep.versions.stable.as_deref().unwrap_or("N/A"))
                );
            }
        }

        if !plan.to_install.is_empty() {
             println!("\n{}", self.formatter.section("Packages to be installed:"));
            for pkg in &plan.to_install {
                println!("  {} {} {}", 
                    self.formatter.bullet_available(),
                    self.formatter.package_name(&pkg.name, false),
                    self.formatter.package_version(pkg.versions.stable.as_deref().unwrap_or("N/A"))
                );
            }
        }

        if !plan.warnings.is_empty() {
            println!("\n{}", self.formatter.section("Warnings:"));
            for warning in &plan.warnings {
                println!("  - {}", self.formatter.warning(warning));
            }
        }
    }
}