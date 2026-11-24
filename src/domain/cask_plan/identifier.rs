//! Validation of cask targets and identifiers.

use super::error::IdentifierError;

/// Official cask tap supported by the native backend.
pub(super) const CASK_TAP: &str = "homebrew/cask";

/// Maximum length of a cask token.
pub(super) const MAX_TOKEN_LEN: usize = 128;

/// Validate a cask CLI target (`<token>` or `homebrew/cask/<token>`).
pub(crate) fn parse_cask_target(raw: &str) -> Result<&str, IdentifierError> {
    if raw.len() > CASK_TAP.len() + 1 + MAX_TOKEN_LEN {
        return Err(IdentifierError::TargetTooLong {
            raw: raw.to_owned(),
        });
    }
    let token = match raw.rsplit_once('/') {
        Some((tap, token)) => {
            if tap != CASK_TAP {
                return Err(IdentifierError::UnsupportedTap {
                    tap: tap.to_owned(),
                    raw: raw.to_owned(),
                });
            }
            token
        }
        None => raw,
    };
    validate_cask_identifier(token, "target cask token")
}

/// Validate one lowercase Homebrew identifier (cask token or formula name).
pub(crate) fn validate_cask_identifier<'a>(
    value: &'a str,
    what: &'static str,
) -> Result<&'a str, IdentifierError> {
    if value.is_empty() || value.len() > MAX_TOKEN_LEN {
        return Err(IdentifierError::Length {
            what,
            value: value.to_owned(),
        });
    }
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => {
            return Err(IdentifierError::Start {
                what,
                value: value.to_owned(),
            });
        }
    }
    if value.contains("..") {
        return Err(IdentifierError::DotDot {
            what,
            value: value.to_owned(),
        });
    }
    if !value.chars().all(|ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '@' | '+' | '-' | '_' | '.')
    }) {
        return Err(IdentifierError::Characters {
            what,
            value: value.to_owned(),
        });
    }
    Ok(value)
}
