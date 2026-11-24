use super::error::{HostArchError, NameError};
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
pub(crate) fn parse_qualified(raw: &str) -> Result<QualifiedTarget<'_>, NameError> {
    if raw.is_empty() || raw.len() > 2 * MAX_TAP_COMPONENT_LEN + 1 + MAX_TOKEN_LEN {
        return Err(NameError::TargetLength {
            raw: raw.to_owned(),
        });
    }
    let not_qualified = || NameError::NotQualified {
        raw: raw.to_owned(),
    };
    let mut parts = raw.split('/');
    let Some(owner) = parts.next().filter(|part| !part.is_empty()) else {
        return Err(not_qualified());
    };
    let Some(repo) = parts.next().filter(|part| !part.is_empty()) else {
        return Err(not_qualified());
    };
    let Some(token) = parts.next().filter(|part| !part.is_empty()) else {
        return Err(not_qualified());
    };
    if parts.next().is_some() {
        return Err(NameError::TooManyComponents {
            raw: raw.to_owned(),
        });
    }
    validate_tap_component(owner, "tap owner")?;
    validate_tap_component(repo, "tap repository")?;
    if is_reserved_owner(owner) {
        return Err(NameError::ReservedOwner {
            owner: owner.to_owned(),
        });
    }
    if is_reserved_repo(repo) {
        return Err(NameError::ReservedRepository {
            name: format!("{owner}/{repo}"),
        });
    }
    validate_token_identifier(token)?;
    Ok(QualifiedTarget { owner, repo, token })
}

/// Parse and validate a `<owner>/<repo>` tap name.
pub(crate) fn parse_tap_name(raw: &str) -> Result<(String, String), NameError> {
    if raw.is_empty() || raw.len() > 2 * MAX_TAP_COMPONENT_LEN + 1 {
        return Err(NameError::TapNameLength {
            raw: raw.to_owned(),
        });
    }
    let Some((owner, repo)) = raw.split_once('/') else {
        return Err(NameError::TapNameShape {
            raw: raw.to_owned(),
        });
    };
    if repo.contains('/') {
        return Err(NameError::TapNameSlash {
            raw: raw.to_owned(),
        });
    }
    validate_tap_component(owner, "tap owner")?;
    validate_tap_component(repo, "tap repository")?;
    if is_reserved_owner(owner) {
        return Err(NameError::ReservedOwner {
            owner: owner.to_owned(),
        });
    }
    if is_reserved_repo(repo) {
        return Err(NameError::ReservedRepository {
            name: raw.to_owned(),
        });
    }
    Ok((owner.to_string(), repo.to_string()))
}

pub(super) fn validate_tap_component(value: &str, what: &'static str) -> Result<(), NameError> {
    let value_owned = || value.to_owned();
    if value.is_empty() || value.len() > MAX_TAP_COMPONENT_LEN {
        return Err(NameError::Length {
            what,
            value: value_owned(),
            max: MAX_TAP_COMPONENT_LEN,
        });
    }
    if value.contains("..") {
        return Err(NameError::DotDot {
            what,
            value: value_owned(),
        });
    }
    if value.starts_with('.') || value.ends_with('.') {
        return Err(NameError::EdgeDot {
            what,
            value: value_owned(),
        });
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => {
            return Err(NameError::StartLowerOrDigit {
                what,
                value: value_owned(),
            });
        }
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
    }) {
        return Err(NameError::ComponentCharacters {
            what,
            value: value_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_token_identifier(value: &str) -> Result<(), NameError> {
    validate_token_syntax(value)?;
    if RESERVED_PACKAGE_TOKENS.contains(&value) {
        return Err(NameError::Reserved {
            what: "package token",
            value: value.to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_token_syntax(value: &str) -> Result<(), NameError> {
    const WHAT: &str = "package token";
    let value_owned = || value.to_owned();
    if value.is_empty() || value.len() > MAX_TOKEN_LEN {
        return Err(NameError::Length {
            what: WHAT,
            value: value_owned(),
            max: MAX_TOKEN_LEN,
        });
    }
    if value.contains("..") {
        return Err(NameError::DotDot {
            what: WHAT,
            value: value_owned(),
        });
    }
    if value.starts_with('.') || value.ends_with('.') {
        return Err(NameError::EdgeDot {
            what: WHAT,
            value: value_owned(),
        });
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => {
            return Err(NameError::StartLowerOrDigit {
                what: WHAT,
                value: value_owned(),
            });
        }
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        return Err(NameError::IdentifierCharacters {
            what: WHAT,
            value: value_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_bin_identifier(value: &str) -> Result<(), NameError> {
    validate_bin_syntax(value)?;
    if RESERVED_BIN_NAMES.contains(&value) {
        return Err(NameError::Reserved {
            what: "executable name",
            value: value.to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_bin_syntax(value: &str) -> Result<(), NameError> {
    const WHAT: &str = "executable name";
    let value_owned = || value.to_owned();
    if value.is_empty() || value.len() > MAX_BIN_LEN {
        return Err(NameError::Length {
            what: WHAT,
            value: value_owned(),
            max: MAX_BIN_LEN,
        });
    }
    if value.contains("..") {
        return Err(NameError::DotDot {
            what: WHAT,
            value: value_owned(),
        });
    }
    if value.starts_with('.') || value.ends_with('.') {
        return Err(NameError::EdgeDot {
            what: WHAT,
            value: value_owned(),
        });
    }
    let mut characters = value.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() || first == '_' => {}
        _ => {
            return Err(NameError::StartLowerDigitOrUnderscore {
                what: WHAT,
                value: value_owned(),
            });
        }
    }
    if !characters.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '@' | '+' | '-' | '_' | '.')
    }) {
        return Err(NameError::IdentifierCharacters {
            what: WHAT,
            value: value_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_version(value: &str) -> Result<(), NameError> {
    const WHAT: &str = "version";
    let value_owned = || value.to_owned();
    if value.is_empty() || value.len() > MAX_VERSION_LEN {
        return Err(NameError::Length {
            what: WHAT,
            value: value_owned(),
            max: MAX_VERSION_LEN,
        });
    }
    if value.contains("..") {
        return Err(NameError::DotDot {
            what: WHAT,
            value: value_owned(),
        });
    }
    if value.ends_with('.') {
        return Err(NameError::TrailingDot {
            what: WHAT,
            value: value_owned(),
        });
    }
    if !value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
    {
        return Err(NameError::StartAlphanumeric {
            what: WHAT,
            value: value_owned(),
        });
    }
    if !value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '-')
    }) {
        return Err(NameError::VersionCharacters {
            what: WHAT,
            value: value_owned(),
        });
    }
    Ok(())
}

/// Whether `architecture` is one of the two supported macOS target names.
pub(crate) fn is_known_arch(architecture: &str) -> bool {
    matches!(architecture, "arm64" | "x86_64")
}

/// The host architecture name used to select a manifest artifact.
pub(crate) fn host_arch() -> Result<&'static str, HostArchError> {
    if std::env::consts::OS != "macos" {
        return Err(HostArchError::NotMacos {
            os: std::env::consts::OS,
        });
    }
    match std::env::consts::ARCH {
        "aarch64" => Ok("arm64"),
        "x86_64" => Ok("x86_64"),
        arch => Err(HostArchError::UnsupportedArch { arch }),
    }
}
