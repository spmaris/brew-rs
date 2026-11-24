//! The official formula API as the planner's [`FormulaSource`].

use anyhow::Result;

use crate::config::Config;
use crate::domain::plan::{
    FormulaResponse, FormulaSource, InstalledServiceMetadata, validate_formula_name, validate_tag,
    verify_installed_service,
};
use crate::infra::api::error::error_chain;
use crate::infra::api::http_client::HttpClient;
use crate::infra::api::{API_BASE, ApiError, catalog_cache};

/// [`FormulaSource`] backed by the official API over [`HttpClient`].
pub(crate) struct HttpFormulaSource {
    http: HttpClient,
    alias_config: Option<Config>,
}

impl HttpFormulaSource {
    /// A source that falls back to the cached catalog for alias names.
    pub(crate) fn new(config: &Config) -> Result<Self, ApiError> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: Some(config.clone()),
        })
    }

    /// A source that never resolves aliases, so it never reads or writes a
    /// catalog cache.
    pub(crate) fn read_only(config: &Config) -> Result<Self, ApiError> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: None,
        })
    }

    async fn fetch_payload(&self, name: &str) -> Result<FormulaResponse, ApiError> {
        let url = format!("{API_BASE}/formula/{name}.json");
        match self.http.get_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if error.is_not_found()
                    && let Some(config) = &self.alias_config
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_formula_alias(config, name).await
                    && validate_formula_name(&canonical, "alias target").is_ok()
                {
                    let alias_url = format!("{API_BASE}/formula/{canonical}.json");
                    return self.http.get_json(&alias_url).await.map_err(|source| {
                        ApiError::FormulaAliasFetch {
                            canonical,
                            alias: name.to_owned(),
                            source: Box::new(source),
                        }
                    });
                }
                log::debug!("GET {url} failed: {}", error_chain(&error));
                Err(ApiError::FormulaFetch {
                    name: name.to_owned(),
                    source: Box::new(error),
                })
            }
        }
    }
}

/// Hand the planner a domain failure instead of a transport error when the
/// chain is a not-found, unreachable or 5xx failure.
fn into_planner_error(error: ApiError) -> anyhow::Error {
    match error.failure() {
        Some(failure) => anyhow::Error::new(failure),
        None => anyhow::Error::new(error),
    }
}

impl FormulaSource for HttpFormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
        self.fetch_payload(name).await.map_err(|error| {
            log::debug!(
                "formula lookup for '{name}' failed: {}",
                error_chain(&error)
            );
            into_planner_error(error)
        })
    }
}

/// Fetch official metadata for `name` and require it to describe exactly the
/// installed receipt.
///
/// This is a read-only metadata query. It deliberately bypasses alias
/// resolution and the catalog cache, so rendering a preview never populates or
/// mutates a cache. The checks themselves live in
/// [`verify_installed_service`].
pub(crate) async fn fetch_installed_service(
    config: &Config,
    name: &str,
    tag: &str,
    installed_version: &str,
    installed_version_scheme: u32,
    installed_sha256: &str,
) -> Result<InstalledServiceMetadata> {
    validate_tag(tag)?;
    let name = validate_formula_name(name, "service formula name")?;
    let http = HttpClient::new(config)?;
    let url = format!("{API_BASE}/formula/{name}.json");
    let response: FormulaResponse =
        http.get_json(&url)
            .await
            .map_err(|source| ApiError::FormulaFetchFrom {
                name: name.to_owned(),
                url,
                source: Box::new(source),
            })?;
    verify_installed_service(
        response,
        name,
        tag,
        installed_version,
        installed_version_scheme,
        installed_sha256,
    )
}

#[cfg(test)]
mod tests;
