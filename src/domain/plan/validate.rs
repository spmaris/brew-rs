//! Validation of untrusted names, tags, versions, digests, URLs and cellars.

use std::collections::HashMap;

use super::error::{MetadataError, NameError};
use super::model::BottleFile;

/// Tap supported by this planner.
pub(super) const CORE_TAP: &str = "homebrew/core";
/// Host that serves official Homebrew bottles.
pub(super) const BOTTLE_HOST: &str = "ghcr.io";
/// Path prefix shared by official `homebrew/core` bottle blobs.
const BOTTLE_URL_PREFIX: &str = "https://ghcr.io/v2/homebrew/core/";

/// Maximum length of a formula name.
pub(super) const MAX_NAME_LEN: usize = 64;
/// Maximum length of a rendered version.
const MAX_VERSION_LEN: usize = 64;
/// Maximum length of a host tag.
pub(super) const MAX_TAG_LEN: usize = 64;
/// Maximum length of a bottle URL.
const MAX_URL_LEN: usize = 2048;
/// Maximum length of a bottle cellar path.
const MAX_CELLAR_LEN: usize = 1024;
/// Length of a SHA-256 digest in hexadecimal.
const SHA256_HEX_LEN: usize = 64;

/// Parse a target into a bare formula name, rejecting unsupported taps.
pub(crate) fn parse_target(raw: &str) -> Result<&str, NameError> {
    if raw.is_empty() {
        return Err(NameError::TargetEmpty);
    }
    if raw.len() > MAX_NAME_LEN + CORE_TAP.len() + 1 {
        return Err(NameError::TargetTooLong {
            raw: raw.to_owned(),
        });
    }

    let name = match raw.rsplit_once('/') {
        Some((tap, name)) => {
            if tap != CORE_TAP {
                return Err(NameError::UnsupportedTap {
                    tap: tap.to_owned(),
                    raw: raw.to_owned(),
                });
            }
            name
        }
        None => raw,
    };

    validate_formula_name(name, "target formula name")
}

/// Validate a host tag: lowercase ASCII, digits and underscores only.
pub(crate) fn validate_tag(tag: &str) -> Result<(), NameError> {
    if tag.is_empty() || tag.len() > MAX_TAG_LEN {
        return Err(NameError::TagLength);
    }
    let mut characters = tag.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => {
            return Err(NameError::TagStart {
                tag: tag.to_owned(),
            });
        }
    }
    if !tag.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
    }) {
        return Err(NameError::TagCharacters {
            tag: tag.to_owned(),
        });
    }
    Ok(())
}

/// Validate a Homebrew formula identifier and return it unchanged.
pub(crate) fn validate_formula_name<'a>(
    value: &'a str,
    what: &'static str,
) -> Result<&'a str, NameError> {
    if value.is_empty() || value.len() > MAX_NAME_LEN {
        return Err(NameError::Length {
            what,
            value: value.to_owned(),
        });
    }

    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => {
            return Err(NameError::Start {
                what,
                value: value.to_owned(),
            });
        }
    }
    if value.contains("..") {
        return Err(NameError::DotDot {
            what,
            value: value.to_owned(),
        });
    }
    if !value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        return Err(NameError::Characters {
            what,
            value: value.to_owned(),
        });
    }
    Ok(value)
}

/// Validate a rendered version string.
pub(super) fn validate_version(version: &str, formula: &str) -> Result<(), MetadataError> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        return Err(MetadataError::VersionLength {
            formula: formula.to_owned(),
        });
    }
    if !version.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
    }) {
        return Err(MetadataError::InvalidVersion {
            formula: formula.to_owned(),
            version: version.to_owned(),
        });
    }
    Ok(())
}

/// Validate and normalize a bottle digest to lowercase hexadecimal.
pub(super) fn validate_sha256(value: &str, formula: &str) -> Result<String, MetadataError> {
    if value.len() != SHA256_HEX_LEN
        || !value.chars().all(|character| character.is_ascii_hexdigit())
    {
        return Err(MetadataError::InvalidSha256 {
            formula: formula.to_owned(),
        });
    }
    Ok(value.to_ascii_lowercase())
}

/// Validate the bottle URL and require it to agree with `sha256`.
pub(super) fn validate_bottle_url(
    value: &str,
    sha256: &str,
    formula: &str,
) -> Result<String, MetadataError> {
    let formula = || formula.to_owned();
    if value.is_empty() || value.len() > MAX_URL_LEN {
        return Err(MetadataError::BottleUrlLength { formula: formula() });
    }
    if !value.is_ascii()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(MetadataError::BottleUrlCharacters { formula: formula() });
    }
    let Some(rest) = value.strip_prefix("https://") else {
        return Err(MetadataError::BottleUrlNotHttps { formula: formula() });
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority != BOTTLE_HOST {
        return Err(MetadataError::BottleUrlHost { formula: formula() });
    }
    let Some(rest) = value.strip_prefix(BOTTLE_URL_PREFIX) else {
        return Err(MetadataError::BottleUrlNotOfficial { formula: formula() });
    };
    let Some((repository, embedded)) = rest.rsplit_once("/blobs/sha256:") else {
        return Err(MetadataError::BottleUrlNoBlob { formula: formula() });
    };
    if repository.is_empty()
        || repository.split('/').any(|segment| {
            segment.is_empty()
                || segment == "."
                || segment == ".."
                || !segment.chars().all(|character| {
                    character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || matches!(character, '@' | '+' | '-' | '_' | '.')
                })
        })
    {
        return Err(MetadataError::BottleUrlRepository { formula: formula() });
    }
    if embedded != sha256 {
        return Err(MetadataError::BottleUrlDigest { formula: formula() });
    }
    Ok(value.to_string())
}

/// Validate the bottle cellar: the two Homebrew sentinels or an absolute,
/// normalized path.
pub(super) fn validate_cellar(value: &str, formula: &str) -> Result<String, MetadataError> {
    let formula = || formula.to_owned();
    if value == ":any" || value == ":any_skip_relocation" {
        return Ok(value.to_string());
    }
    if value.is_empty() || value.len() > MAX_CELLAR_LEN {
        return Err(MetadataError::CellarLength { formula: formula() });
    }
    if !value.is_ascii()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(MetadataError::CellarCharacters { formula: formula() });
    }
    if !value.starts_with('/') || value.contains('\\') || value.contains("//") {
        return Err(MetadataError::CellarNotAbsolute { formula: formula() });
    }
    let components: Vec<&str> = value[1..].split('/').collect();
    if components.is_empty()
        || components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        return Err(MetadataError::CellarNotNormalized { formula: formula() });
    }
    Ok(value.to_string())
}

/// Render the available bottle tags for an error message, deterministically.
pub(super) fn available_tags(files: &HashMap<String, BottleFile>) -> String {
    if files.is_empty() {
        return "none".to_string();
    }
    let mut tags: Vec<&str> = files.keys().map(String::as_str).collect();
    tags.sort_unstable();
    tags.join(", ")
}
