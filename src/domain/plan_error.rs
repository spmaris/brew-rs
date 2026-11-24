//! Typed errors for formula lookups against the official API.
//!
//! The planner maps raw API failures to [`PlanError`], whose `Display` is the
//! whole user-facing message (no error chain). The raw chain, with URLs, is
//! logged at debug level instead. Transport errors are classified by the
//! infrastructure layer, which hands the planner an [`ApiFailure`].

use std::fmt;
use std::path::PathBuf;

/// How a Homebrew API request failed, for user-facing error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiFailure {
    NotFound,
    Unreachable,
    Server(u16),
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "formulae.brew.sh has no such entry (HTTP 404)"),
            Self::Unreachable => write!(
                f,
                "Could not reach formulae.brew.sh. Check your network connection."
            ),
            Self::Server(code) => write!(
                f,
                "formulae.brew.sh returned an error (HTTP {code}). Try again later."
            ),
        }
    }
}

impl std::error::Error for ApiFailure {}

#[derive(Debug)]
pub(crate) enum PlanError {
    /// A user-requested formula does not exist. `tap` is set for a
    /// tap-qualified target.
    NotFound {
        name: String,
        tap: Option<String>,
        hints: Vec<String>,
    },
    /// A formula's metadata lists a dependency the API does not know.
    MissingDependency {
        parent: String,
        dependency: String,
    },
    Api(ApiFailure),
    /// A lookup failed in a way that is not not-found, network or 5xx; the
    /// raw chain is only in the debug log.
    MetadataUnreadable {
        name: String,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound {
                name,
                tap: Some(tap),
                hints,
            } => {
                write!(f, "No formula named '{name}' in tap {tap}.")?;
                hints.iter().try_for_each(|hint| write!(f, "\n  {hint}"))
            }
            Self::NotFound {
                name,
                tap: None,
                hints,
            } => {
                write!(f, "No Homebrew formula named '{name}'.")?;
                hints.iter().try_for_each(|hint| write!(f, "\n  {hint}"))
            }
            Self::MissingDependency { parent, dependency } => write!(
                f,
                "Homebrew metadata for '{parent}' lists dependency '{dependency}', which does not exist."
            ),
            Self::Api(failure) => failure.fmt(f),
            Self::MetadataUnreadable { name } => write!(
                f,
                "Could not read Homebrew metadata for '{name}'.\n  Run with BREW_RS_LOG=debug for details."
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// The [`ApiFailure`] an error chain carries, if any.
pub(crate) fn api_failure(error: &anyhow::Error) -> Option<ApiFailure> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ApiFailure>().copied())
}

/// Convert a failed fetch of `name` into the friendly error. `parent` is the
/// formula that listed `name` as a dependency, or `None` for a user target.
/// Failures that are not not-found/network/5xx become a readable metadata
/// error; the original chain is logged at debug level.
pub(crate) fn from_fetch_error(
    error: &anyhow::Error,
    name: &str,
    tap: Option<&str>,
    parent: Option<&str>,
) -> PlanError {
    let Some(failure) = api_failure(error) else {
        log::debug!("formula lookup for '{name}' failed: {error:#}");
        return PlanError::MetadataUnreadable { name: name.into() };
    };
    match (failure, parent) {
        (ApiFailure::NotFound, Some(parent)) => PlanError::MissingDependency {
            parent: parent.into(),
            dependency: name.into(),
        },
        (ApiFailure::NotFound, None) => PlanError::NotFound {
            name: name.into(),
            tap: tap.map(str::to_owned),
            hints: Vec::new(),
        },
        (failure, _) => PlanError::Api(failure),
    }
}

/// Facts about a missing formula's name that need I/O to learn.
pub(crate) trait HintProbe {
    /// A regular, executable macOS system file named exactly `name`.
    fn macos_builtin(&self, name: &str) -> Option<PathBuf>;
    /// Whether a cask with this token exists. Failures count as "no".
    async fn cask_exists(&self, token: &str) -> bool;
}

#[cfg(test)]
mod tests;
