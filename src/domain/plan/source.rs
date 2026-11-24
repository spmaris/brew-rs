//! The formula metadata port and its audit-scoped memoizing wrapper.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::super::plan_error::{ApiFailure, api_failure};
use super::model::FormulaResponse;

pub(super) const MAX_AUDIT_REQUESTS: usize = 512;

/// A [`FormulaSource`] that reuses metadata across independently resolved
/// formulae. The cache is scoped to one audit; readiness never writes a
/// metadata cache.
pub(crate) struct Memoized<S> {
    pub(super) source: S,
    pub(super) cached:
        std::cell::RefCell<HashMap<String, std::result::Result<FormulaResponse, String>>>,
}

impl<S> Memoized<S> {
    pub(crate) fn new(source: S) -> Self {
        Self {
            source,
            cached: Default::default(),
        }
    }
}

impl<S: FormulaSource> FormulaSource for Memoized<S> {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse> {
        if let Some(cached) = self.cached.borrow().get(name).cloned() {
            return cached.map_err(anyhow::Error::msg);
        }
        if self.cached.borrow().len() >= MAX_AUDIT_REQUESTS {
            bail!("Migration audit exceeded the {MAX_AUDIT_REQUESTS}-request metadata budget");
        }
        let answer = self.source.fetch(name).await.map_err(|error| {
            if api_failure(&error) == Some(ApiFailure::NotFound) {
                format!(
                    "No current official formula record for '{name}' (HTTP 404). It may have been renamed or removed; this audit does not resolve aliases."
                )
            } else {
                format!("{error:#}")
            }
        });
        self.cached.borrow_mut().insert(name.into(), answer.clone());
        answer.map_err(anyhow::Error::msg)
    }
}

/// A source of formula metadata: the port the resolver reads through.
///
/// Implementations report transport failures as an [`ApiFailure`] in the error
/// chain so the planner can render them without knowing the transport.
pub(crate) trait FormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse>;
}
