//! Validation of cask targets and identifiers.

use anyhow::{Result, bail};

/// Official cask tap supported by the native backend.
pub(super) const CASK_TAP: &str = "homebrew/cask";

/// Maximum length of a cask token.
pub(super) const MAX_TOKEN_LEN: usize = 128;

/// Validate a cask CLI target (`<token>` or `homebrew/cask/<token>`).
pub(crate) fn parse_cask_target(raw: &str) -> Result<&str> {
    if raw.len() > CASK_TAP.len() + 1 + MAX_TOKEN_LEN {
        bail!("cask target '{raw}' is longer than the supported maximum");
    }
    let token = match raw.rsplit_once('/') {
        Some((tap, token)) => {
            if tap != CASK_TAP {
                bail!(
                    "unsupported tap '{tap}' in target '{raw}': only {CASK_TAP} casks are supported"
                );
            }
            token
        }
        None => raw,
    };
    validate_cask_identifier(token, "target cask token")
}

/// Validate one lowercase Homebrew identifier (cask token or formula name).
pub(crate) fn validate_cask_identifier<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    if value.is_empty() || value.len() > MAX_TOKEN_LEN {
        bail!("{what} '{value}' must be between 1 and {MAX_TOKEN_LEN} characters");
    }
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => bail!("{what} '{value}' must start with a lowercase letter or digit"),
    }
    if value.contains("..") {
        bail!("{what} '{value}' must not contain '..'");
    }
    if !value.chars().all(|ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '@' | '+' | '-' | '_' | '.')
    }) {
        bail!("{what} '{value}' contains characters outside [a-z0-9@+._-]");
    }
    Ok(value)
}
