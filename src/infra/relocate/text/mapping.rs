//! Mapping of placeholders onto the managed prefix and keg roots.

use anyhow::{Context, Result, bail};

use super::super::paths::Paths;
use super::{CELLAR_MARKER, FIXED_CELLAR, OPT_MARKER};

/// Reject whitespace or quotes in a marker value before mapping it.
pub(super) fn checked_literal<'a>(value: &'a str, context: &str) -> Result<&'a str> {
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value
            .bytes()
            .any(|byte| matches!(byte, b'"' | b'\'' | b'`' | b'\\' | b'$' | b'#'))
    {
        bail!("Unsupported literal path {value:?} in {context}");
    }
    Ok(value)
}

/// Map a placeholder path through the pinned transaction.
pub(super) fn map_keg(paths: &Paths<'_>, placeholder: &str) -> Result<String> {
    match paths.keg_path(placeholder)? {
        Some(mapped) => {
            validate_mapped(&mapped)?;
            Ok(mapped)
        }
        None => bail!("Unsupported Homebrew placeholder path {placeholder:?}"),
    }
}

/// Map a complete `Cellar/<name>/<version>` or `opt/<name>` keg root.
pub(super) fn map_complete_keg_root(paths: &Paths<'_>, value: &str) -> Result<String> {
    let value = checked_literal(value, "pcre2-config prefix assignment")?;
    if !is_complete_keg_root(value) {
        bail!(
            "Unsupported pcre2-config prefix placeholder {value:?}: expected a complete pinned keg"
        );
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
pub(super) fn map_fixed_cellar(paths: &Paths<'_>, value: &str) -> Result<String> {
    let value = checked_literal(value, "pcre2-config libR assignment")?;
    let rest = value
        .strip_prefix(FIXED_CELLAR)
        .with_context(|| format!("Unsupported fixed Homebrew path {value:?}"))?;
    if rest.is_empty() {
        bail!("Unsupported fixed Homebrew path {value:?}");
    }
    map_keg(paths, &format!("{CELLAR_MARKER}{rest}"))
}

/// A mapped path must be an absolute, single-line path.
pub(super) fn validate_mapped(value: &str) -> Result<()> {
    if value.is_empty()
        || !value.starts_with('/')
        || value.bytes().any(|byte| {
            byte < 0x20 || byte == 0x7f || matches!(byte, b'"' | b'\'' | b'`' | b'\\' | b'$')
        })
    {
        bail!("Unsupported mapped path {value:?}");
    }
    Ok(())
}

/// Quote one shell word with single quotes.
pub(super) fn shell_quote(value: &str) -> Result<String> {
    if value
        .bytes()
        .any(|byte| byte < 0x20 || byte == 0x7f || byte == b'\n')
    {
        bail!("Cannot quote a shell value containing control bytes");
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
