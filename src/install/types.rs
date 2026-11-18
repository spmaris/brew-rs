use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::api::types::Formula;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    pub install_time: u64,
    pub dependencies: Vec<String>,
    pub size: u64,
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct InstallPlan {
    pub to_install: Vec<Formula>,
    pub dependencies: Vec<Formula>,
    pub conflicts: Vec<String>,
    pub warnings: Vec<String>,
}

impl InstallPlan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_valid(&self) -> bool {
        self.conflicts.is_empty()
    }


    pub fn add_conflict(&mut self, message: String) {
        self.conflicts.push(message);
    }

    pub fn add_warning(&mut self, message: String) {
        self.warnings.push(message);
    }
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub force: bool,
    pub no_deps: bool,
    pub head: bool,
    pub keep_bottles: bool,
    pub verify_checksums: bool,
}