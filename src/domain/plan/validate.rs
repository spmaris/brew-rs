//! Validation of untrusted names, tags, versions, digests, URLs and cellars.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::model::BottleFile;

/// Tap supported by this planner.
pub(super) const CORE_TAP: &str = "homebrew/core";
/// Host that serves official Homebrew bottles.
const BOTTLE_HOST: &str = "ghcr.io";
/// Path prefix shared by official `homebrew/core` bottle blobs.
const BOTTLE_URL_PREFIX: &str = "https://ghcr.io/v2/homebrew/core/";

/// Maximum length of a formula name.
pub(super) const MAX_NAME_LEN: usize = 64;
/// Maximum length of a rendered version.
const MAX_VERSION_LEN: usize = 64;
/// Maximum length of a host tag.
const MAX_TAG_LEN: usize = 64;
/// Maximum length of a bottle URL.
const MAX_URL_LEN: usize = 2048;
/// Maximum length of a bottle cellar path.
const MAX_CELLAR_LEN: usize = 1024;
/// Length of a SHA-256 digest in hexadecimal.
const SHA256_HEX_LEN: usize = 64;

/// Parse a target into a bare formula name, rejecting unsupported taps.
pub(crate) fn parse_target(raw: &str) -> Result<&str> {
    if raw.is_empty() {
        bail!("target formula names must not be empty");
    }
    if raw.len() > MAX_NAME_LEN + CORE_TAP.len() + 1 {
        bail!("target '{raw}' is longer than the supported maximum");
    }

    let name = match raw.rsplit_once('/') {
        Some((tap, name)) => {
            if tap != CORE_TAP {
                bail!(
                    "unsupported tap '{tap}' in target '{raw}': only {CORE_TAP} formulae are supported"
                );
            }
            name
        }
        None => raw,
    };

    validate_formula_name(name, "target formula name")
}

/// Validate a host tag: lowercase ASCII, digits and underscores only.
pub(crate) fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() || tag.len() > MAX_TAG_LEN {
        bail!("host tag must be between 1 and {MAX_TAG_LEN} characters");
    }
    let mut characters = tag.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("host tag '{tag}' must start with a lowercase letter or digit"),
    }
    if !tag.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
    }) {
        bail!("host tag '{tag}' may only contain lowercase letters, digits and underscores");
    }
    Ok(())
}

/// Validate a Homebrew formula identifier and return it unchanged.
pub(crate) fn validate_formula_name<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    if value.is_empty() || value.len() > MAX_NAME_LEN {
        bail!("{what} '{value}' must be between 1 and {MAX_NAME_LEN} characters");
    }

    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{value}' must start with a lowercase letter or digit"),
    }
    if value.contains("..") {
        bail!("{what} '{value}' must not contain '..'");
    }
    if !value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("{what} '{value}' contains characters outside [a-z0-9@+._-]");
    }
    Ok(value)
}

/// Validate a rendered version string.
pub(super) fn validate_version(version: &str, formula: &str) -> Result<()> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        bail!("formula '{formula}' has an invalid version length");
    }
    if !version.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
    }) {
        bail!("formula '{formula}' has an invalid version '{version}'");
    }
    Ok(())
}

/// Validate and normalize a bottle digest to lowercase hexadecimal.
pub(super) fn validate_sha256(value: &str, formula: &str) -> Result<String> {
    if value.len() != SHA256_HEX_LEN
        || !value.chars().all(|character| character.is_ascii_hexdigit())
    {
        bail!("formula '{formula}' has an invalid bottle sha256");
    }
    Ok(value.to_ascii_lowercase())
}

/// Validate the bottle URL and require it to agree with `sha256`.
pub(super) fn validate_bottle_url(value: &str, sha256: &str, formula: &str) -> Result<String> {
    if value.is_empty() || value.len() > MAX_URL_LEN {
        bail!("formula '{formula}' has an invalid bottle URL length");
    }
    if !value.is_ascii()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!(
            "formula '{formula}' has a bottle URL with whitespace, control or non-ASCII characters"
        );
    }
    let Some(rest) = value.strip_prefix("https://") else {
        bail!("formula '{formula}' bottle URL must use https");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority != BOTTLE_HOST {
        bail!("formula '{formula}' bottle URL must point at {BOTTLE_HOST}");
    }
    let Some(rest) = value.strip_prefix(BOTTLE_URL_PREFIX) else {
        bail!("formula '{formula}' bottle URL must be an official homebrew/core blob");
    };
    let Some((repository, embedded)) = rest.rsplit_once("/blobs/sha256:") else {
        bail!("formula '{formula}' bottle URL must end in /blobs/sha256:<digest>");
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
        bail!("formula '{formula}' bottle URL has an invalid repository path");
    }
    if embedded != sha256 {
        bail!("formula '{formula}' bottle URL digest does not match its sha256");
    }
    Ok(value.to_string())
}

/// Validate the bottle cellar: the two Homebrew sentinels or an absolute,
/// normalized path.
pub(super) fn validate_cellar(value: &str, formula: &str) -> Result<String> {
    if value == ":any" || value == ":any_skip_relocation" {
        return Ok(value.to_string());
    }
    if value.is_empty() || value.len() > MAX_CELLAR_LEN {
        bail!("formula '{formula}' has an invalid bottle cellar");
    }
    if !value.is_ascii()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        bail!("formula '{formula}' bottle cellar contains whitespace or control characters");
    }
    if !value.starts_with('/') || value.contains('\\') || value.contains("//") {
        bail!("formula '{formula}' bottle cellar must be an absolute path");
    }
    let components: Vec<&str> = value[1..].split('/').collect();
    if components.is_empty()
        || components
            .iter()
            .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        bail!("formula '{formula}' bottle cellar is not a normalized path");
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
