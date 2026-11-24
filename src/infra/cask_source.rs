//! The official cask API as the planner's [`CaskSource`].

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::Config;
use crate::domain::cask_plan::{CaskSource, validate_cask_identifier};
use crate::infra::api::http_client::{HttpClient, is_not_found};
use crate::infra::api::{API_BASE, catalog_cache};

/// [`CaskSource`] backed by the official API over [`HttpClient`].
pub(crate) struct HttpCaskSource {
    http: HttpClient,
    alias_config: Option<Config>,
}

impl HttpCaskSource {
    /// A source that falls back to the cached catalog for renamed tokens.
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: Some(config.clone()),
        })
    }

    /// A source that never resolves renames, so it never reads or writes a
    /// catalog cache.
    pub(crate) fn read_only(config: &Config) -> Result<Self> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: None,
        })
    }
}

impl CaskSource for HttpCaskSource {
    async fn fetch_cask(&self, token: &str) -> Result<Value> {
        let url = format!("{API_BASE}/cask/{token}.json");
        match self.http.fetch_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if is_not_found(&error)
                    && let Some(config) = &self.alias_config
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_cask_old_token(config, token).await
                    && !canonical.contains('/')
                    && validate_cask_identifier(&canonical, "cask old_token").is_ok()
                {
                    let alias_url = format!("{API_BASE}/cask/{canonical}.json");
                    return self.http.fetch_json(&alias_url).await.with_context(|| {
                                    format!(
                                        "failed to fetch cask '{canonical}' (resolved from '{token}') from {alias_url}"
                                    )
                                });
                }
                Err(error).with_context(|| format!("failed to fetch cask '{token}' from {url}"))
            }
        }
    }
}
