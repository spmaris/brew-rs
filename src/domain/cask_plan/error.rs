//! Typed refusals of the cask planner.
//!
//! Every `Display` text is the exact message the planner has always produced.

use super::identifier::{CASK_TAP, MAX_TOKEN_LEN};
use super::resolve::MAX_TARGETS;

/// Why a cask target or identifier was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum IdentifierError {
    #[error("cask target '{raw}' is longer than the supported maximum")]
    TargetTooLong { raw: String },
    #[error("unsupported tap '{tap}' in target '{raw}': only {CASK_TAP} casks are supported")]
    UnsupportedTap { tap: String, raw: String },
    #[error("{what} '{value}' must be between 1 and {MAX_TOKEN_LEN} characters")]
    Length { what: &'static str, value: String },
    #[error("{what} '{value}' must start with a lowercase letter or digit")]
    Start { what: &'static str, value: String },
    #[error("{what} '{value}' must not contain '..'")]
    DotDot { what: &'static str, value: String },
    #[error("{what} '{value}' contains characters outside [a-z0-9@+._-]")]
    Characters { what: &'static str, value: String },
}

/// Why cask metadata or the set of requested casks was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CaskPlanError {
    #[error(transparent)]
    Identifier(#[from] IdentifierError),
    #[error("at least one cask target is required")]
    NoTargets,
    #[error("cannot resolve more than {MAX_TARGETS} cask targets at once (got {count})")]
    TooManyTargets { count: usize },
    #[error("cask API response is not a JSON object")]
    NotAnObject,
    #[error("failed to parse Homebrew cask metadata")]
    ParseMetadata(#[source] serde_json::Error),
    #[error("Homebrew API returned cask '{requested}' without a token")]
    MissingToken { requested: String },
    #[error("cask '{token}' comes from unsupported tap '{tap}'; only {CASK_TAP} is supported")]
    UnsupportedTap { token: String, tap: String },
    #[error("cask '{token}' is disabled ({reason})")]
    Disabled { token: String, reason: String },
    #[error("cask '{token}' does not declare a string version")]
    VersionNotString { token: String },
    #[error("cask '{token}' has an invalid version length")]
    VersionLength { token: String },
    #[error("cask '{token}' has an unsafe version string '{raw}'")]
    UnsafeVersion { token: String, raw: String },
}
