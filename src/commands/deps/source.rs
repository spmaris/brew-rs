use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::domain::plan;
use crate::infra::api::catalog_cache;
use crate::infra::api::http_client::HttpClient;

/// Official Homebrew formula API base.
pub(super) const API_BASE: &str = "https://formulae.brew.sh/api";

/// Tap supported by this query, as in the native planner.
pub(super) const CORE_TAP: &str = "homebrew/core";

/// [`plan`]'s online formula source, using the shared HTTP client.
pub(super) struct HttpFormulaSource {
    pub(super) http: HttpClient,
    pub(super) config: Config,
}

impl FormulaSource for HttpFormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaPayload> {
        // `name` comes from `plan::parse_target`, so it cannot alter the path.
        let url = format!("{API_BASE}/formula/{name}.json");
        match self.http.get_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if error.is_not_found()
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_formula_alias(&self.config, name).await
                    && !canonical.contains('/')
                    && plan::parse_target(&canonical).is_ok()
                {
                    let alias_url = format!("{API_BASE}/formula/{canonical}.json");
                    return self.http.get_json(&alias_url).await.with_context(|| {
                        format!("failed to fetch formula metadata from {alias_url}")
                    });
                }
                Err(error).with_context(|| format!("failed to fetch formula metadata from {url}"))
            }
        }
    }
}

/// A source of formula metadata, kept as a seam for offline fixture tests.
pub(super) trait FormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaPayload>;
}

/// The subset of the official formula payload used by this query.
#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct FormulaPayload {
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) tap: String,
    #[serde(default)]
    pub(super) versions: FormulaVersions,
    #[serde(default)]
    pub(super) revision: Option<u32>,
    #[serde(default)]
    pub(super) dependencies: Vec<String>,
    #[serde(default)]
    pub(super) build_dependencies: Vec<Value>,
    #[serde(default)]
    pub(super) test_dependencies: Vec<Value>,
    #[serde(default)]
    pub(super) recommended_dependencies: Vec<Value>,
    #[serde(default)]
    pub(super) optional_dependencies: Vec<Value>,
    #[serde(default)]
    pub(super) uses_from_macos: Vec<Value>,
    #[serde(default)]
    pub(super) variations: HashMap<String, Variation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct FormulaVersions {
    #[serde(default)]
    pub(super) stable: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct Variation {
    /// `None` means the variation does not override dependencies; `Some(vec![])`
    /// means it explicitly removes them.
    #[serde(default)]
    pub(super) dependencies: Option<Vec<String>>,
}
