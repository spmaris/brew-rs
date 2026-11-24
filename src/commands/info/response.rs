use std::collections::BTreeMap;

use serde::Deserialize;

use super::analytics::InlineAnalytics;

/// A formula as published by `formula/<name>.json`, reduced to displayed
/// fields.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct FormulaResponse {
    pub(super) name: String,
    pub(super) full_name: String,
    pub(super) tap: String,
    pub(super) desc: Option<String>,
    pub(super) license: Option<String>,
    pub(super) homepage: Option<String>,
    pub(super) versions: FormulaVersions,
    pub(super) revision: Option<u32>,
    pub(super) keg_only: bool,
    pub(super) keg_only_reason: Option<KegOnlyReason>,
    pub(super) caveats: Option<String>,
    pub(super) deprecated: bool,
    pub(super) deprecation_date: Option<String>,
    pub(super) deprecation_reason: Option<String>,
    pub(super) deprecation_replacement_formula: Option<String>,
    pub(super) deprecation_replacement_cask: Option<String>,
    pub(super) disabled: bool,
    pub(super) disable_date: Option<String>,
    pub(super) disable_reason: Option<String>,
    pub(super) disable_replacement_formula: Option<String>,
    pub(super) disable_replacement_cask: Option<String>,
    pub(super) dependencies: Vec<String>,
    pub(super) build_dependencies: Vec<String>,
    pub(super) recommended_dependencies: Vec<String>,
    pub(super) optional_dependencies: Vec<String>,
    pub(super) uses_from_macos: Vec<serde_json::Value>,
    pub(super) conflicts_with: Vec<String>,
    pub(super) aliases: Vec<String>,
    pub(super) oldnames: Vec<String>,
    pub(super) analytics: Option<InlineAnalytics>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct FormulaVersions {
    pub(super) stable: Option<String>,
    pub(super) head: Option<String>,
    pub(super) bottle: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct KegOnlyReason {
    pub(super) reason: String,
    pub(super) explanation: String,
}

/// A cask as published by `cask/<token>.json`, reduced to displayed fields.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct CaskResponse {
    pub(super) token: String,
    pub(super) full_token: String,
    pub(super) old_tokens: Vec<String>,
    pub(super) tap: String,
    pub(super) name: Vec<String>,
    pub(super) desc: Option<String>,
    pub(super) homepage: Option<String>,
    pub(super) version: String,
    pub(super) auto_updates: Option<bool>,
    pub(super) caveats: Option<String>,
    pub(super) deprecated: bool,
    pub(super) deprecation_date: Option<String>,
    pub(super) deprecation_reason: Option<String>,
    pub(super) deprecation_replacement_formula: Option<String>,
    pub(super) deprecation_replacement_cask: Option<String>,
    pub(super) disabled: bool,
    pub(super) disable_date: Option<String>,
    pub(super) disable_reason: Option<String>,
    pub(super) disable_replacement_formula: Option<String>,
    pub(super) disable_replacement_cask: Option<String>,
    pub(super) depends_on: serde_json::Value,
    pub(super) conflicts_with: serde_json::Value,
    pub(super) analytics: Option<InlineAnalytics>,
}

/// A tap-scoped analytics report as published by
/// `analytics/<category>/<tap>/<period>.json`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct AnalyticsResponse {
    pub(super) category: String,
    pub(super) total_items: u64,
    pub(super) start_date: String,
    pub(super) end_date: String,
    pub(super) total_count: u64,
    pub(super) formulae: BTreeMap<String, Vec<AnalyticsEntry>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct AnalyticsEntry {
    pub(super) formula: Option<String>,
    pub(super) cask: Option<String>,
    pub(super) count: String,
}
