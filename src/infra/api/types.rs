use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A robust representation of a Homebrew formula, designed to ignore
/// unknown fields and gracefully handle the complex API structure.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct Formula {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub versions: Versions,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub oldnames: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub build_dependencies: Vec<String>,
    #[serde(default)]
    pub keg_only: bool,
    pub keg_only_reason: Option<KegOnlyReason>,
    pub caveats: Option<String>,
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub disabled: bool,

    // Use `serde_json::Value` for deeply nested or complex fields
    // that we don't need to interact with directly. This prevents parsing errors.
    #[serde(default)]
    pub bottle: Option<serde_json::Value>,
    #[serde(default)]
    pub variations: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct Cask {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub old_tokens: Vec<String>,
    #[serde(default)]
    pub name: Vec<String>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct Versions {
    #[serde(default)]
    pub stable: Option<String>,
    pub head: Option<String>,
    #[serde(default)]
    pub bottle: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct KegOnlyReason {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub explanation: String,
}

impl std::fmt::Display for KegOnlyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)?;
        if !self.explanation.is_empty() {
            write!(f, " ({})", self.explanation)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct Analytics {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub total_items: u32,
    #[serde(default)]
    pub start_date: String,
    #[serde(default)]
    pub end_date: String,
    #[serde(default)]
    pub total_count: u64,
    pub items: Option<Vec<AnalyticsItem>>,
    pub formulae: Option<HashMap<String, Vec<AnalyticsFormula>>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct AnalyticsItem {
    #[serde(default)]
    pub number: u32,
    #[serde(default)]
    pub formula: String,
    #[serde(default)]
    pub count: String,
    #[serde(default)]
    pub percent: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AnalyticsFormula {
    pub formula: Option<String>,
    pub cask: Option<String>,
    pub count: String,
}

#[cfg(test)]
mod tests;
