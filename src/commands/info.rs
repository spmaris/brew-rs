//! Native package information and install analytics.
//!
//! `info` and `analytics` read the official Homebrew JSON API at
//! `https://formulae.brew.sh/api` through [`HttpClient`]. Homebrew is never
//! executed, no Ruby is evaluated, and no package state is changed.
//!
//! # Namespaces and lookup order
//!
//! A bare name is looked up as a formula first. Only an *explicit* `HTTP 404`
//! from the formula endpoint triggers the cask lookup, so a network failure, a
//! 5xx response or a malformed payload is surfaced instead of being reported as
//! "package not found". When both namespaces contain the same name the formula
//! is shown deterministically; pass `--cask` (or `homebrew/cask/<token>`) to
//! inspect the cask. A tap-qualified identifier must use one of the two
//! official taps, `homebrew/core` or `homebrew/cask`, and must agree with an
//! explicit `--formula`/`--cask` flag.
//!
//! [`HttpClient`] preserves the typed `reqwest::StatusCode` on its error chain,
//! so the HTTP source classifies a failure as "not found" only when a
//! `reqwest::Error` in that chain carries `404 Not Found`. Error text is never
//! inspected, and every other failure is propagated with its context. When a
//! formula endpoint returns `404`, [`HttpMetadataSource`] checks the formula
//! catalog for an exact alias or old name before falling back to casks.
//!
//! # Data shown
//!
//! * Formula: description, tap, stable/head versions (with the `_revision`
//!   suffix), bottle availability, license, homepage, aliases, old names,
//!   keg-only state, conflicts, direct/recommended/optional/build dependencies,
//!   `uses_from_macos`, deprecation or disable state, caveats, and the inline
//!   per-period install counts published with the formula payload.
//! * Cask: human name, description, tap, version (and auto-updating state),
//!   homepage, old tokens, `depends_on`, conflicts, deprecation or disable
//!   state, caveats, and the inline install counts.
//! * Installed state: overlaid from native formula and cask receipts in the
//!   selected prefix.
//!
//! `info --analytics` renders the package payload's inline install map. The
//! standalone [`analytics`] function reads the tap-scoped analytics endpoint
//! (`/analytics/<category>/<tap>/<period>.json`), which adds the reporting
//! window and the tap total. Only the periods the API publishes (`30d`, `90d`
//! and `365d`) are accepted; anything else is rejected before a request.

use std::path::Path;

use anyhow::Result;

use super::install::prefix_path;
use crate::config::Config;

use self::analytics::print_analytics;
use self::render::print_info;
use self::request::{parse_package, requested_kind, validate_period};
use self::source::{HttpMetadataSource, MetadataSource, lookup};

mod analytics;
mod fields;
mod installed;
mod render;
mod request;
mod response;
mod source;
mod status;

pub(super) use self::render::caveat_block;

/// Show metadata for one formula or cask.
///
/// `formula` and `cask` mirror the CLI's `--formula`/`--cask` flags and are
/// mutually exclusive. When neither is set, a bare name is resolved as a
/// formula first and as a cask only after an explicit 404. `deps` adds the
/// dependency tables, and `analytics` adds the inline install counts.
pub(crate) async fn info(
    config: &Config,
    prefix: Option<&Path>,
    package: &str,
    formula: bool,
    cask: bool,
    deps: bool,
    analytics: bool,
) -> Result<()> {
    let requested = parse_package(package)?;
    let kind = requested_kind(package, &requested, formula, cask)?;
    let source = HttpMetadataSource::new(config)?;
    let metadata = lookup(&source, requested.name, kind).await?;
    let prefix = prefix_path(prefix)?;
    print_info(config, &prefix, &metadata, deps, analytics);
    Ok(())
}

/// Show tap-scoped install analytics for one formula or cask.
///
/// The namespace is inferred the same way as [`info`]: an explicit tap selects
/// it, otherwise the name is resolved as a formula first and as a cask only
/// after an explicit 404. Only `30d`, `90d` and `365d` are accepted.
pub(crate) async fn analytics(config: &Config, package: &str, period: &str) -> Result<()> {
    let period = validate_period(period)?;
    let requested = parse_package(package)?;
    let source = HttpMetadataSource::new(config)?;
    let metadata = lookup(&source, requested.name, requested.kind).await?;
    let response = source.analytics(metadata.kind, period).await?;
    print_analytics(config, &metadata, period, &response)
}

#[cfg(test)]
mod tests;
