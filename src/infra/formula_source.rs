//! The official formula API as the planner's [`FormulaSource`].

use anyhow::{Context, Result};

use crate::config::Config;
use crate::domain::plan::{
    FormulaResponse, FormulaSource, InstalledServiceMetadata, validate_formula_name, validate_tag,
    verify_installed_service,
};
use crate::infra::api::http_client::{HttpClient, classify, is_not_found};
use crate::infra::api::{API_BASE, catalog_cache};

/// [`FormulaSource`] backed by the official API over [`HttpClient`].
pub(crate) struct HttpFormulaSource {
    http: HttpClient,
    alias_config: Option<Config>,
}

impl HttpFormulaSource {
    /// A source that falls back to the cached catalog for alias names.
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: Some(config.clone()),
        })
    }

    /// A source that never resolves aliases, so it never reads or writes a
    /// catalog cache.
    pub(crate) fn read_only(config: &Config) -> Result<Self> {
        Ok(Self {
            http: HttpClient::new(config)?,
            alias_config: None,
        })
    }

    async fn fetch_payload(&self, name: &str) -> Result<FormulaResponse> {
        let url = format!("{API_BASE}/formula/{name}.json");
        match self.http.fetch_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if is_not_found(&error)
                    && let Some(config) = &self.alias_config
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_formula_alias(config, name).await
                    && validate_formula_name(&canonical, "alias target").is_ok()
                {
                    let alias_url = format!("{API_BASE}/formula/{canonical}.json");
                    return self.http.fetch_json(&alias_url).await.with_context(|| {
                        format!(
                            "failed to fetch formula '{canonical}' (resolved from alias '{name}')"
                        )
                    });
                }
                log::debug!("GET {url} failed: {error:#}");
                Err(error).with_context(|| format!("failed to fetch formula '{name}'"))
            }
        }
    }
}

impl FormulaSource for HttpFormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
        self.fetch_payload(name).await.map_err(|error| {
            // Hand the planner a domain failure instead of a transport error.
            match classify(&error) {
                Some(failure) => {
                    log::debug!("formula lookup for '{name}' failed: {error:#}");
                    anyhow::Error::new(failure)
                }
                None => error,
            }
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
    let response: FormulaResponse = http
        .fetch_json(&url)
        .await
        .with_context(|| format!("failed to fetch formula '{name}' from {url}"))?;
    verify_installed_service(
        response,
        name,
        tag,
        installed_version,
        installed_version_scheme,
        installed_sha256,
    )
}
