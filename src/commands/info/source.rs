use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::infra::api::http_client::HttpClient;
use crate::infra::api::{ApiError, catalog_cache};

use super::request::{Kind, validate_identifier};
use super::response::{AnalyticsResponse, CaskResponse, FormulaResponse};

/// Official Homebrew JSON API base.
pub(super) const API_BASE: &str = "https://formulae.brew.sh/api";

/// The only formula tap supported by the native backend.
pub(super) const CORE_TAP: &str = "homebrew/core";

/// The only cask tap supported by the native backend.
pub(super) const CASK_TAP: &str = "homebrew/cask";

/// Why a per-package lookup produced no payload.
#[derive(Debug)]
pub(super) enum LookupError {
    /// The endpoint returned an explicit HTTP 404.
    NotFound,
    /// Anything else: transport failure, 5xx, malformed JSON, ...
    Failed(anyhow::Error),
}

/// A seam over the package endpoints so the lookup rules and formatting can be
/// tested offline.
pub(super) trait MetadataSource {
    async fn formula(&self, name: &str) -> std::result::Result<FormulaResponse, LookupError>;
    async fn cask(&self, token: &str) -> std::result::Result<CaskResponse, LookupError>;
    async fn analytics(&self, kind: Kind, period: &str) -> Result<AnalyticsResponse>;
}

/// [`MetadataSource`] backed by the official API over [`HttpClient`].
pub(super) struct HttpMetadataSource {
    pub(super) http: HttpClient,
    pub(super) config: Config,
}

impl HttpMetadataSource {
    pub(super) fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            http: HttpClient::new(config)?,
            config: config.clone(),
        })
    }
}

impl MetadataSource for HttpMetadataSource {
    async fn formula(&self, name: &str) -> std::result::Result<FormulaResponse, LookupError> {
        let url = format!("{API_BASE}/{}/{name}.json", Kind::Formula.endpoint());
        match self.http.get_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if error.is_not_found()
                    && let Ok(Some(canonical)) =
                        catalog_cache::resolve_formula_alias(&self.config, name).await
                    && !canonical.contains('/')
                    && validate_identifier(&canonical, "formula alias").is_ok()
                {
                    let alias_url =
                        format!("{API_BASE}/{}/{canonical}.json", Kind::Formula.endpoint());
                    return match self.http.get_json(&alias_url).await {
                        Ok(payload) => Ok(payload),
                        Err(alias_error) => Err(classify_http_error(&alias_url, alias_error)),
                    };
                }
                Err(classify_http_error(&url, error))
            }
        }
    }

    async fn cask(&self, token: &str) -> std::result::Result<CaskResponse, LookupError> {
        let url = format!("{API_BASE}/{}/{token}.json", Kind::Cask.endpoint());
        match self.http.get_json(&url).await {
            Ok(payload) => Ok(payload),
            Err(error) => Err(classify_http_error(&url, error)),
        }
    }

    async fn analytics(&self, kind: Kind, period: &str) -> Result<AnalyticsResponse> {
        let url = analytics_url(kind, period);
        self.http
            .get_json(&url)
            .await
            .with_context(|| format!("Could not fetch Homebrew analytics from {url}"))
    }
}

/// Turn a [`HttpClient`] failure into a typed lookup outcome.
///
/// Only a typed `404 Not Found` status is treated as "not found"; a transport
/// error, a 5xx status, a parse failure or an untyped message keeps its error
/// and its context.
pub(super) fn classify_http_error(url: &str, error: ApiError) -> LookupError {
    if error.is_not_found() {
        LookupError::NotFound
    } else {
        LookupError::Failed(anyhow::Error::new(error).context(format!("GET {url} failed")))
    }
}

/// Build the tap-scoped analytics endpoint for a period.
pub(super) fn analytics_url(kind: Kind, period: &str) -> String {
    format!(
        "{API_BASE}/analytics/{}/{}/{period}.json",
        kind.analytics_category(),
        kind.analytics_tap()
    )
}

/// Resolved package metadata.
#[derive(Debug)]
pub(super) struct Metadata {
    pub(super) kind: Kind,
    /// Canonical API name: the formula name or cask token.
    pub(super) name: String,
    pub(super) formula: Option<FormulaResponse>,
    pub(super) cask: Option<CaskResponse>,
}

impl Metadata {
    pub(super) fn from_formula(response: FormulaResponse, requested: &str) -> Result<Self> {
        if response.name.is_empty() {
            bail!("The Homebrew API returned formula metadata without a name for '{requested}'");
        }
        validate_identifier(&response.name, "formula name")?;
        Ok(Self {
            kind: Kind::Formula,
            name: response.name.clone(),
            formula: Some(response),
            cask: None,
        })
    }

    pub(super) fn from_cask(response: CaskResponse, requested: &str) -> Result<Self> {
        if response.token.is_empty() {
            bail!("The Homebrew API returned cask metadata without a token for '{requested}'");
        }
        validate_identifier(&response.token, "cask token")?;
        Ok(Self {
            kind: Kind::Cask,
            name: response.token.clone(),
            formula: None,
            cask: Some(response),
        })
    }
}

/// Resolve metadata with the deterministic formula-first rule.
pub(super) async fn lookup<S: MetadataSource>(
    source: &S,
    name: &str,
    kind: Option<Kind>,
) -> Result<Metadata> {
    match kind {
        Some(Kind::Formula) => formula_metadata(source, name).await,
        Some(Kind::Cask) => cask_metadata(source, name).await,
        None => match source.formula(name).await {
            Ok(formula) => Metadata::from_formula(formula, name),
            // Only a real 404 permits the cask fallback.
            Err(LookupError::NotFound) => match source.cask(name).await {
                Ok(cask) => Metadata::from_cask(cask, name),
                Err(LookupError::NotFound) => {
                    bail!(
                        "No formula or cask named '{name}' was found in the official Homebrew API"
                    )
                }
                Err(LookupError::Failed(error)) => {
                    Err(error.context(format!("Could not fetch cask '{name}' from {CASK_TAP}")))
                }
            },
            Err(LookupError::Failed(error)) => {
                Err(error.context(format!("Could not fetch formula '{name}' from {CORE_TAP}")))
            }
        },
    }
}

pub(super) async fn formula_metadata<S: MetadataSource>(
    source: &S,
    name: &str,
) -> Result<Metadata> {
    match source.formula(name).await {
        Ok(formula) => Metadata::from_formula(formula, name),
        Err(LookupError::NotFound) => {
            bail!("No formula named '{name}' was found in {CORE_TAP}")
        }
        Err(LookupError::Failed(error)) => {
            Err(error.context(format!("Could not fetch formula '{name}' from {CORE_TAP}")))
        }
    }
}

pub(super) async fn cask_metadata<S: MetadataSource>(source: &S, token: &str) -> Result<Metadata> {
    match source.cask(token).await {
        Ok(cask) => Metadata::from_cask(cask, token),
        Err(LookupError::NotFound) => {
            bail!("No cask named '{token}' was found in {CASK_TAP}")
        }
        Err(LookupError::Failed(error)) => {
            Err(error.context(format!("Could not fetch cask '{token}' from {CASK_TAP}")))
        }
    }
}
