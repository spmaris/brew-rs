//! The formula metadata port and its audit-scoped memoizing wrapper.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::super::plan_error::{ApiFailure, api_failure};
use super::model::FormulaResponse;

pub(super) const MAX_AUDIT_REQUESTS: usize = 512;

/// A failed fetch, kept cloneable: the classification the planner renders
/// from, next to the full chain text for the debug log.
#[derive(Clone)]
pub(super) struct FetchFailure {
    failure: Option<ApiFailure>,
    message: String,
}

impl FetchFailure {
    fn new(error: &anyhow::Error) -> Self {
        Self {
            failure: api_failure(error),
            message: format!("{error:#}"),
        }
    }

    /// An error carrying the original classification, so the planner renders
    /// the same friendly text as an uncached lookup.
    fn to_error(&self) -> anyhow::Error {
        match self.failure {
            Some(failure) => anyhow::Error::new(failure).context(self.message.clone()),
            None => anyhow::anyhow!("{}", self.message),
        }
    }
}

/// A [`FormulaSource`] that reuses metadata across independently resolved
/// formulae. The cache is scoped to one audit; readiness never writes a
/// metadata cache.
pub(crate) struct Memoized<S> {
    pub(super) source: S,
    pub(super) cached:
        std::cell::RefCell<HashMap<String, std::result::Result<FormulaResponse, FetchFailure>>>,
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
            return cached.map_err(|failure| failure.to_error());
        }
        if self.cached.borrow().len() >= MAX_AUDIT_REQUESTS {
            bail!("Migration audit exceeded the {MAX_AUDIT_REQUESTS}-request metadata budget");
        }
        let answer = self
            .source
            .fetch(name)
            .await
            .map_err(|error| FetchFailure::new(&error));
        self.cached.borrow_mut().insert(name.into(), answer.clone());
        answer.map_err(|failure| failure.to_error())
    }
}

/// A source of formula metadata: the port the resolver reads through.
///
/// Implementations report transport failures as an [`ApiFailure`] in the error
/// chain so the planner can render them without knowing the transport.
pub(crate) trait FormulaSource {
    async fn fetch(&self, name: &str) -> Result<FormulaResponse>;
}
