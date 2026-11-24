//! Typed refusals of the formula planner.
//!
//! Every `Display` text is the exact message the planner has always produced.

use super::node::MAX_DEPS_PER_FORMULA;
use super::resolve::{MAX_FORMULAE, MAX_REQUESTS, MAX_TARGETS};
use super::validate::{BOTTLE_HOST, CORE_TAP, MAX_NAME_LEN, MAX_TAG_LEN};
use crate::domain::plan_error::PlanError;

/// Why a target, host tag or formula identifier was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NameError {
    #[error("target formula names must not be empty")]
    TargetEmpty,
    #[error("target '{raw}' is longer than the supported maximum")]
    TargetTooLong { raw: String },
    #[error("unsupported tap '{tap}' in target '{raw}': only {CORE_TAP} formulae are supported")]
    UnsupportedTap { tap: String, raw: String },
    #[error("host tag must be between 1 and {MAX_TAG_LEN} characters")]
    TagLength,
    #[error("host tag '{tag}' must start with a lowercase letter or digit")]
    TagStart { tag: String },
    #[error("host tag '{tag}' may only contain lowercase letters, digits and underscores")]
    TagCharacters { tag: String },
    #[error("{what} '{value}' must be between 1 and {MAX_NAME_LEN} characters")]
    Length { what: &'static str, value: String },
    #[error("{what} '{value}' must start with a lowercase letter or digit")]
    Start { what: &'static str, value: String },
    #[error("{what} '{value}' must not contain '..'")]
    DotDot { what: &'static str, value: String },
    #[error("{what} '{value}' contains characters outside [a-z0-9@+._-]")]
    Characters { what: &'static str, value: String },
}

/// Why an API formula payload was refused by the planner.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MetadataError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error("unsupported tap '{tap}' for formula '{name}': only {CORE_TAP} is supported")]
    UnsupportedTap { tap: String, name: String },
    #[error("formula '{name}' is disabled and cannot be installed")]
    Disabled { name: String },
    #[error(
        "formula '{name}' has no bottled stable version (source-only builds are not supported)"
    )]
    NoBottledStable { name: String },
    #[error("formula '{name}' has no stable version")]
    NoStableVersion { name: String },
    #[error("formula '{name}' has an empty stable version")]
    EmptyStableVersion { name: String },
    #[error("formula '{name}' has no stable bottle metadata")]
    NoStableBottle { name: String },
    #[error("formula '{name}' has no bottle for host tag '{tag}' (available: {available})")]
    NoBottleForTag {
        name: String,
        tag: String,
        available: String,
    },
    #[error(
        "formula '{name}' declares {count} dependencies, above the {MAX_DEPS_PER_FORMULA} limit"
    )]
    TooManyDependencies { name: String, count: usize },
    #[error("formula '{formula}' has an invalid version length")]
    VersionLength { formula: String },
    #[error("formula '{formula}' has an invalid version '{version}'")]
    InvalidVersion { formula: String, version: String },
    #[error("formula '{formula}' has an invalid bottle sha256")]
    InvalidSha256 { formula: String },
    #[error("formula '{formula}' has an invalid bottle URL length")]
    BottleUrlLength { formula: String },
    #[error(
        "formula '{formula}' has a bottle URL with whitespace, control or non-ASCII characters"
    )]
    BottleUrlCharacters { formula: String },
    #[error("formula '{formula}' bottle URL must use https")]
    BottleUrlNotHttps { formula: String },
    #[error("formula '{formula}' bottle URL must point at {BOTTLE_HOST}")]
    BottleUrlHost { formula: String },
    #[error("formula '{formula}' bottle URL must be an official homebrew/core blob")]
    BottleUrlNotOfficial { formula: String },
    #[error("formula '{formula}' bottle URL must end in /blobs/sha256:<digest>")]
    BottleUrlNoBlob { formula: String },
    #[error("formula '{formula}' bottle URL has an invalid repository path")]
    BottleUrlRepository { formula: String },
    #[error("formula '{formula}' bottle URL digest does not match its sha256")]
    BottleUrlDigest { formula: String },
    #[error("formula '{formula}' has an invalid bottle cellar")]
    CellarLength { formula: String },
    #[error("formula '{formula}' bottle cellar contains whitespace or control characters")]
    CellarCharacters { formula: String },
    #[error("formula '{formula}' bottle cellar must be an absolute path")]
    CellarNotAbsolute { formula: String },
    #[error("formula '{formula}' bottle cellar is not a normalized path")]
    CellarNotNormalized { formula: String },
}

/// Why official metadata did not describe an installed receipt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(
        "official metadata for '{name}' returned formula '{canonical}'; refusing to preview a different formula"
    )]
    WrongFormula { name: String, canonical: String },
    #[error("formula '{name}' is served by tap '{tap}'; only {CORE_TAP} metadata is supported")]
    WrongTap { name: String, tap: String },
    #[error("formula '{name}' is disabled upstream")]
    Disabled { name: String },
    #[error("formula '{name}' has no bottled stable version")]
    NoBottledStable { name: String },
    #[error(
        "installed receipt pins {name} {installed}, but official metadata reports {official}; refusing to preview a different version"
    )]
    VersionMismatch {
        name: String,
        installed: String,
        official: String,
    },
    #[error(
        "installed receipt pins {name} version scheme {installed}, but official metadata reports {official:?}; refusing to preview a different version"
    )]
    SchemeMismatch {
        name: String,
        installed: u32,
        official: Option<u32>,
    },
    #[error(
        "installed receipt pins {name} bottle {installed}, but official metadata for tag '{tag}' reports {official}; refusing to preview a different artifact"
    )]
    DigestMismatch {
        name: String,
        installed: String,
        tag: String,
        official: String,
    },
    #[error("formula '{name}' does not declare a service")]
    NoService { name: String },
    #[error(
        "formula '{name}' declares a service declaration this native installer does not recognize as optional"
    )]
    UnrecognizedService { name: String },
}

/// Why no default bottle tag exists for a macOS release or architecture.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MacosTagError {
    #[error("Unknown macOS bottle tag for {version}; pass --target")]
    UnknownRelease { version: String },
    #[error("No default Homebrew bottle tag for {arch}; pass --target")]
    UnknownArch { arch: String },
}

/// Why a plan for the requested formulae could not be built.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    /// A lookup failed; the message is the friendly one for the failure.
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("too many targets: {count} requested, at most {MAX_TARGETS} are supported")]
    TooManyTargets { count: usize },
    #[error("inconsistent formula metadata: '{canonical}' was already resolved as '{existing}'")]
    InconsistentMetadata { canonical: String, existing: String },
    #[error("install plan exceeds the {MAX_FORMULAE}-formula limit")]
    FormulaLimit,
    #[error("install plan exceeds the {MAX_REQUESTS}-request limit")]
    RequestLimit,
    #[error("internal planning error: unresolved target '{name}'")]
    UnresolvedTarget { name: String },
    #[error("internal planning error: missing node '{name}'")]
    MissingNode { name: String },
    #[error("internal planning error: unresolved dependency '{dependency}' of '{parent}'")]
    UnresolvedDependency { dependency: String, parent: String },
    #[error("dependency cycle detected involving formula '{name}'")]
    DependencyCycle { name: String },
}
