//! Mapping of placeholders onto the managed prefix and keg roots.

use super::super::paths::Paths;
use super::error::TextError;
use super::{CELLAR_MARKER, FIXED_CELLAR, OPT_MARKER};

/// Reject whitespace or quotes in a marker value before mapping it.
pub(super) fn checked_literal<'a>(
    value: &'a str,
    context: &'static str,
) -> Result<&'a str, TextError> {
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value
            .bytes()
            .any(|byte| matches!(byte, b'"' | b'\'' | b'`' | b'\\' | b'$' | b'#'))
    {
        return Err(TextError::UnsupportedLiteral {
            value: value.to_owned(),
            context,
        });
    }
    Ok(value)
}

/// Map a placeholder path through the pinned transaction.
pub(super) fn map_keg(paths: &Paths<'_>, placeholder: &str) -> Result<String, TextError> {
    match paths.keg_path(placeholder)? {
        Some(mapped) => {
            validate_mapped(&mapped)?;
            Ok(mapped)
        }
        None => Err(TextError::UnsupportedPlaceholderPath {
            placeholder: placeholder.to_owned(),
        }),
    }
}

/// Map a complete `Cellar/<name>/<version>` or `opt/<name>` keg root.
pub(super) fn map_complete_keg_root(paths: &Paths<'_>, value: &str) -> Result<String, TextError> {
    let value = checked_literal(value, "pcre2-config prefix assignment")?;
    if !is_complete_keg_root(value) {
        return Err(TextError::UnsupportedPrefixPlaceholder {
            value: value.to_owned(),
        });
    }
    map_keg(paths, value)
}

/// Whether a placeholder names exactly one keg and nothing below it.
fn is_complete_keg_root(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix(CELLAR_MARKER) {
        let mut parts = rest.split('/');
        return matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some(name), Some(version), None)
                if !name.is_empty() && !version.is_empty()
        );
    }
    if let Some(rest) = value.strip_prefix(OPT_MARKER) {
        return !rest.is_empty() && !rest.contains('/');
    }
    false
}

/// Map one fixed `/opt/homebrew/Cellar/...` path through the same pins.
pub(super) fn map_fixed_cellar(paths: &Paths<'_>, value: &str) -> Result<String, TextError> {
    let value = checked_literal(value, "pcre2-config libR assignment")?;
    let rest = value
        .strip_prefix(FIXED_CELLAR)
        .ok_or_else(|| TextError::UnsupportedFixedPath {
            value: value.to_owned(),
        })?;
    if rest.is_empty() {
        return Err(TextError::UnsupportedFixedPath {
            value: value.to_owned(),
        });
    }
    map_keg(paths, &format!("{CELLAR_MARKER}{rest}"))
}

/// A mapped path must be an absolute, single-line path.
pub(super) fn validate_mapped(value: &str) -> Result<(), TextError> {
    if value.is_empty()
        || !value.starts_with('/')
        || value.bytes().any(|byte| {
            byte < 0x20 || byte == 0x7f || matches!(byte, b'"' | b'\'' | b'`' | b'\\' | b'$')
        })
    {
        return Err(TextError::UnsupportedMappedPath {
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Quote one shell word with single quotes.
pub(super) fn shell_quote(value: &str) -> Result<String, TextError> {
    if value
        .bytes()
        .any(|byte| byte < 0x20 || byte == 0x7f || byte == b'\n')
    {
        return Err(TextError::UnquotableShellValue);
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    Ok(quoted)
}
