use anyhow::{Result, bail};

use super::limits::{MAX_BIN_LEN, MAX_TAP_COMPONENT_LEN, MAX_TOKEN_LEN, MAX_VERSION_LEN};
use super::reserved::{
    RESERVED_BIN_NAMES, RESERVED_PACKAGE_TOKENS, is_reserved_owner, is_reserved_repo,
};

/// Parsed form of a fully qualified `<owner>/<repo>/<token>` target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QualifiedTarget<'a> {
    pub(crate) owner: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) token: &'a str,
}

impl QualifiedTarget<'_> {
    /// The `<owner>/<repo>` tap name.
    pub(crate) fn tap(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Parse and validate a fully qualified target. Bare and two-component
/// targets are refused so a tap package can never silently resolve to an
/// official formula or cask.
pub(crate) fn parse_qualified(raw: &str) -> Result<QualifiedTarget<'_>> {
    if raw.is_empty() || raw.len() > 2 * MAX_TAP_COMPONENT_LEN + 1 + MAX_TOKEN_LEN {
        bail!("tap target '{raw}' has an unsupported length");
    }
    let mut parts = raw.split('/');
    let Some(owner) = parts.next().filter(|part| !part.is_empty()) else {
        bail!("tap target '{raw}' is not fully qualified; use <owner>/<repo>/<token>");
    };
    let Some(repo) = parts.next().filter(|part| !part.is_empty()) else {
        bail!("tap target '{raw}' is not fully qualified; use <owner>/<repo>/<token>");
    };
    let Some(token) = parts.next().filter(|part| !part.is_empty()) else {
        bail!("tap target '{raw}' is not fully qualified; use <owner>/<repo>/<token>");
    };
    if parts.next().is_some() {
        bail!("tap target '{raw}' has too many path components; use <owner>/<repo>/<token>");
    }
    validate_tap_component(owner, "tap owner")?;
    validate_tap_component(repo, "tap repository")?;
    if is_reserved_owner(owner) {
        bail!("tap owner '{owner}' is reserved for official Homebrew taps");
    }
    if is_reserved_repo(repo) {
        bail!("tap repository '{owner}/{repo}' is reserved");
    }
    validate_token_identifier(token)?;
    Ok(QualifiedTarget { owner, repo, token })
}

/// Parse and validate a `<owner>/<repo>` tap name.
pub(crate) fn parse_tap_name(raw: &str) -> Result<(String, String)> {
    if raw.is_empty() || raw.len() > 2 * MAX_TAP_COMPONENT_LEN + 1 {
        bail!("tap name '{raw}' has an unsupported length");
    }
    let Some((owner, repo)) = raw.split_once('/') else {
        bail!("tap name '{raw}' must be <owner>/<repo>");
    };
    if repo.contains('/') {
        bail!("tap name '{raw}' must contain exactly one '/'");
    }
    validate_tap_component(owner, "tap owner")?;
    validate_tap_component(repo, "tap repository")?;
    if is_reserved_owner(owner) {
        bail!("tap owner '{owner}' is reserved for official Homebrew taps");
    }
    if is_reserved_repo(repo) {
        bail!("tap repository '{raw}' is reserved");
    }
    Ok((owner.to_string(), repo.to_string()))
}

pub(super) fn validate_tap_component(value: &str, what: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TAP_COMPONENT_LEN {
        bail!("{what} '{value}' must be between 1 and {MAX_TAP_COMPONENT_LEN} characters");
    }
    if value.contains("..") {
        bail!("{what} '{value}' must not contain '..'");
    }
    if value.starts_with('.') || value.ends_with('.') {
        bail!("{what} '{value}' must not start or end with '.'");
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{value}' must start with a lowercase letter or digit"),
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
    }) {
        bail!("{what} '{value}' may only contain lowercase letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

pub(super) fn validate_token_identifier(value: &str) -> Result<()> {
    validate_token_syntax(value)?;
    if RESERVED_PACKAGE_TOKENS.contains(&value) {
        bail!("package token '{value}' is reserved");
    }
    Ok(())
}

pub(super) fn validate_token_syntax(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_TOKEN_LEN {
        bail!("package token '{value}' must be between 1 and {MAX_TOKEN_LEN} characters");
    }
    if value.contains("..") {
        bail!("package token '{value}' must not contain '..'");
    }
    if value.starts_with('.') || value.ends_with('.') {
        bail!("package token '{value}' must not start or end with '.'");
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("package token '{value}' must start with a lowercase letter or digit"),
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("package token '{value}' may only contain [a-z0-9@+._-]");
    }
    Ok(())
}

pub(super) fn validate_bin_identifier(value: &str) -> Result<()> {
    validate_bin_syntax(value)?;
    if RESERVED_BIN_NAMES.contains(&value) {
        bail!("executable name '{value}' is reserved");
    }
    Ok(())
}

pub(super) fn validate_bin_syntax(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_BIN_LEN {
        bail!("executable name '{value}' must be between 1 and {MAX_BIN_LEN} characters");
    }
    if value.contains("..") {
        bail!("executable name '{value}' must not contain '..'");
    }
    if value.starts_with('.') || value.ends_with('.') {
        bail!("executable name '{value}' must not start or end with '.'");
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() || first == '_' => {}
        _ => bail!("executable name '{value}' must start with a lowercase letter, digit or '_'"),
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("executable name '{value}' may only contain [a-z0-9@+._-]");
    }
    Ok(())
}

pub(super) fn validate_version(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_VERSION_LEN {
        bail!("version '{value}' must be between 1 and {MAX_VERSION_LEN} characters");
    }
    if value.contains("..") {
        bail!("version '{value}' must not contain '..'");
    }
    if value.ends_with('.') {
        bail!("version '{value}' must not end with '.'");
    }
    if !value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
    {
        bail!("version '{value}' must start with a letter or digit");
    }
    if !value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
    }) {
        bail!("version '{value}' may only contain [A-Za-z0-9._+-]");
    }
    Ok(())
}

/// Whether `architecture` is one of the two supported macOS target names.
pub(crate) fn is_known_arch(architecture: &str) -> bool {
    matches!(architecture, "arm64" | "x86_64")
}

/// The host architecture name used to select a manifest artifact.
pub(crate) fn host_arch() -> Result<&'static str> {
    if std::env::consts::OS != "macos" {
        bail!(
            "third-party tap artifacts are naked macOS executables; this host is {}",
            std::env::consts::OS
        );
    }
    match std::env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("x86_64"),
        other => bail!("unsupported host architecture '{other}' for macOS tap artifacts"),
    }
}
