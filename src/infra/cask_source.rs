//! The official cask API as the planner's [`CaskSource`].

use anyhow::Result;
use serde_json::Value;

use crate::config::Config;
use crate::domain::cask_plan::{CaskSource, validate_cask_identifier};
use crate::infra::api::http_client::HttpClient;
use crate::infra::api::{API_BASE, ApiError, catalog_cache};

/// [`CaskSource`] backed by the official API over [`HttpClient`].
pub(crate) struct HttpCaskSource {
    http: HttpClient,
    alias_config: Option<Config>,
}

impl HttpCaskSource {
    /// A source that falls back to the cached catalog for renamed tokens.
    pub(crate) fn new(config: &Config) -> Result<Self, ApiError> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: Some(config.clone()),
        })
    }

    /// A source that never resolves renames, so it never reads or writes a
    /// catalog cache.
    pub(crate) fn read_only(config: &Config) -> Result<Self, ApiError> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: None,
        })
    }
}

impl CaskSource for HttpCaskSource {
    async fn fetch_cask(&self, token: &str) -> Result<Value> {
        let url = format!("{API_BASE}/cask/{token}.json");
        match self.http.get_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if error.is_not_found()
                    && let Some(config) = &self.alias_config
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_cask_old_token(config, token).await
                    && !canonical.contains('/')
                    && validate_cask_identifier(&canonical, "cask old_token").is_ok()
                {
                    let url = format!("{API_BASE}/cask/{canonical}.json");
                    return self.http.get_json(&url).await.map_err(|source| {
                        anyhow::Error::new(ApiError::CaskAliasFetch {
                            canonical,
                            token: token.to_owned(),
                            url,
                            source: Box::new(source),
                        })
                    });
                }
                Err(anyhow::Error::new(ApiError::CaskFetch {
                    token: token.to_owned(),
                    url,
                    source: Box::new(error),
                }))
            }
        }
    }
}
