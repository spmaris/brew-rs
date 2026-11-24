//! Conservative version ordering for the native preview.
//!
//! The native backend only understands the plain numeric versions that
//! Homebrew's stable formula API returns, optionally followed by a decimal
//! revision suffix (`1.2.3`, `1.2.3_4`). This module compares exactly that
//! shape and rejects everything else instead of guessing at Homebrew's Ruby
//! `Version` semantics.
//!
//! * Numeric components are compared as arbitrary-precision decimal integers:
//!   they are never parsed into a fixed-width integer, so there is no
//!   overflow. Leading zeros are ignored, and missing or trailing zero
//!   components are equal (`1.0.0` equals `1`). The parent must still refuse
//!   two textually different versions that compare equal when it requires an
//!   exact receipt version.
//! * The `_revision` suffix is compared only after the numeric bases are
//!   equal. An omitted revision is zero (`1.2.3` equals `1.2.3_0`).
//! * Prerelease, build-metadata, date-with-dashes and other non-numeric
//!   syntax is rejected with an error naming the offending version. So are
//!   whitespace, non-ASCII text, empty components and versions beyond the
//!   length and component caps below.
//!
//! Homebrew's `version_scheme` is separate upstream state and is not part of
//! this comparison; the parent checks it before offering an upgrade.

use std::cmp::Ordering;

mod error;

pub(crate) use error::VersionError;

/// Longest version accepted, matching the planner's own version limit.
const MAX_VERSION_LEN: usize = 64;
/// Most numeric base components accepted.
const MAX_COMPONENTS: usize = 16;
/// Most decimal digits accepted after the last `_`.
///
/// A Homebrew revision is an `Integer`; the planner models it as `u32`, whose
/// decimal form is at most ten digits. The cap keeps revisions bounded without
/// parsing them into a machine integer.
const MAX_REVISION_DIGITS: usize = 10;

/// A parsed numeric version borrowed from the caller's string.
struct Version<'a> {
    /// Numeric components of the base version, in order.
    components: Vec<&'a str>,
    /// Decimal revision digits; `"0"` when the suffix was omitted.
    revision: &'a str,
}

/// Compare a candidate version with an installed one.
///
/// Returns [`Ordering::Greater`] when `candidate` is newer, [`Ordering::Less`]
/// when it is older (a downgrade), and [`Ordering::Equal`] when the numeric
/// versions and revisions match. Both versions must use the supported numeric
/// dotted shape; anything else is an error.
pub(crate) fn compare(candidate: &str, installed: &str) -> Result<Ordering, VersionError> {
    let candidate = parse(candidate)?;
    let installed = parse(installed)?;

    let component_count = candidate.components.len().max(installed.components.len());
    for index in 0..component_count {
        let left = candidate.components.get(index).copied().unwrap_or("0");
        let right = installed.components.get(index).copied().unwrap_or("0");
        match compare_decimal(left, right) {
            Ordering::Equal => {}
            ordering => return Ok(ordering),
        }
    }

    // The bases are numerically equal; only now does the revision matter.
    Ok(compare_decimal(candidate.revision, installed.revision))
}

/// Parse the supported `digits(.digits)*(_digits)?` shape.
fn parse(input: &str) -> Result<Version<'_>, VersionError> {
    if input.is_empty() {
        return Err(VersionError::Empty);
    }
    if input.len() > MAX_VERSION_LEN {
        return Err(VersionError::TooLong { len: input.len() });
    }
    if input.chars().any(char::is_whitespace) {
        return Err(VersionError::Whitespace);
    }
    if !input.is_ascii() {
        return Err(VersionError::NonAscii);
    }
    if input.contains(['-', '+']) {
        return Err(VersionError::Prerelease {
            input: input.to_owned(),
            separator: if input.contains('-') { '-' } else { '+' },
        });
    }

    let (base, revision) = match input.split_once('_') {
        Some((base, revision)) => {
            if revision.contains('_') {
                return Err(VersionError::MultipleRevisions {
                    input: input.to_owned(),
                });
            }
            (base, revision)
        }
        None => (input, "0"),
    };

    if base.is_empty() {
        return Err(VersionError::NoBase {
            input: input.to_owned(),
        });
    }
    if revision.is_empty() {
        return Err(VersionError::EmptyRevision {
            input: input.to_owned(),
        });
    }
    if revision.len() > MAX_REVISION_DIGITS {
        return Err(VersionError::RevisionTooLong {
            input: input.to_owned(),
        });
    }
    if !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VersionError::NonDecimalRevision {
            input: input.to_owned(),
        });
    }

    let mut components = Vec::new();
    for component in base.split('.') {
        if component.is_empty() {
            return Err(VersionError::EmptyComponent {
                input: input.to_owned(),
            });
        }
        if !component.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(VersionError::BadComponent {
                input: input.to_owned(),
                component: component.to_owned(),
            });
        }
        components.push(component);
        if components.len() > MAX_COMPONENTS {
            return Err(VersionError::TooManyComponents {
                input: input.to_owned(),
            });
        }
    }

    Ok(Version {
        components,
        revision,
    })
}

/// Compare two non-empty decimal digit strings without parsing them into a
/// fixed-width integer.
fn compare_decimal(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

#[cfg(test)]
mod tests;
