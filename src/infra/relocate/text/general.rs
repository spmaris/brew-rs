//! Line-by-line placeholder rewriting for any other supported UTF-8 text file.

use super::super::paths::Paths;
use super::error::TextError;
use super::mapping::{map_keg, validate_mapped};
use super::{CELLAR_MARKER, MARKER, OPT_MARKER, PREFIX_MARKER, contains};

/// Rewrite one general UTF-8 text file line-by-line through [`rewrite_markers_in_line`].
pub(super) fn rewrite_general_text(text: &str, paths: &Paths<'_>) -> Result<String, TextError> {
    let mut out = String::with_capacity(text.len() + 128);
    for line in text.split_inclusive('\n') {
        if contains(line.as_bytes(), MARKER.as_bytes()) {
            out.push_str(&rewrite_markers_in_line(line, paths)?);
        } else {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// Rewrite every recognized `@@HOMEBREW_*@@` placeholder within one line.
pub(super) fn rewrite_markers_in_line(line: &str, paths: &Paths<'_>) -> Result<String, TextError> {
    let mut out = String::with_capacity(line.len() + 64);
    let mut rest = line;
    while let Some(pos) = rest.find(MARKER) {
        out.push_str(&rest[..pos]);
        let after_marker = &rest[pos..];
        if let Some(tail) = after_marker.strip_prefix(CELLAR_MARKER) {
            let (name, after_name) =
                take_safe_ident(tail).ok_or(TextError::MissingFormulaName {
                    marker: CELLAR_MARKER,
                })?;
            let after_slash =
                after_name
                    .strip_prefix('/')
                    .ok_or_else(|| TextError::MissingSlash {
                        marker: CELLAR_MARKER,
                        name: name.to_owned(),
                    })?;
            let (version, after_ver) =
                take_safe_ident(after_slash).ok_or_else(|| TextError::MissingVersion {
                    marker: CELLAR_MARKER,
                    name: name.to_owned(),
                })?;
            validate_suffix_after_keg(after_ver, false, after_marker)?;
            let placeholder = format!("{CELLAR_MARKER}{name}/{version}");
            let mapped = map_keg(paths, &placeholder)?;
            out.push_str(&mapped);
            rest = after_ver;
        } else if let Some(tail) = after_marker.strip_prefix(OPT_MARKER) {
            let (name, after_name) = take_safe_ident(tail)
                .ok_or(TextError::MissingFormulaName { marker: OPT_MARKER })?;
            validate_suffix_after_keg(after_name, false, after_marker)?;
            let placeholder = format!("{OPT_MARKER}{name}");
            let mapped = map_keg(paths, &placeholder)?;
            out.push_str(&mapped);
            rest = after_name;
        } else if let Some(after_prefix) = after_marker.strip_prefix(PREFIX_MARKER) {
            validate_suffix_after_keg(after_prefix, true, after_marker)?;
            let mapped_prefix = paths.prefix();
            validate_mapped(mapped_prefix)?;
            out.push_str(mapped_prefix);
            rest = after_prefix;
        } else {
            let token_end = after_marker
                .bytes()
                .position(is_token_delimiter)
                .unwrap_or(after_marker.len());
            return Err(TextError::UnsupportedPlaceholder {
                token: after_marker[..token_end].to_owned(),
            });
        }
    }
    out.push_str(rest);
    if out.contains(MARKER) {
        return Err(TextError::UnresolvedPlaceholder);
    }
    Ok(out)
}

fn take_safe_ident(input: &str) -> Option<(&str, &str)> {
    let end = input
        .bytes()
        .position(|byte| !is_safe_ident_byte(byte))
        .unwrap_or(input.len());
    if end == 0 {
        None
    } else {
        Some((&input[..end], &input[end..]))
    }
}

fn is_safe_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+' | b'@')
}

fn is_token_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t'
            | b'\n'
            | b'"'
            | b'\''
            | b';'
            | b':'
            | b','
            | b')'
            | b'('
            | b'>'
            | b'<'
            | b'`'
            | b'}'
            | b'*'
    )
}

/// Ensure what immediately follows a matched placeholder does not concatenate
/// an identifier, reference bare `@@HOMEBREW_PREFIX@@/opt`, or use `..`.
fn validate_suffix_after_keg(
    suffix: &str,
    reject_leading_opt: bool,
    context: &str,
) -> Result<(), TextError> {
    let Some(first) = suffix.bytes().next() else {
        return Ok(());
    };
    if first != b'/' && !is_token_delimiter(first) {
        let preview_len = context.len().min(64);
        return Err(TextError::TrailingToken {
            preview: context[..preview_len].to_owned(),
        });
    }
    let token_end = suffix
        .bytes()
        .position(is_token_delimiter)
        .unwrap_or(suffix.len());
    let path_tail = &suffix[..token_end];
    if let Some(after_slash) = path_tail.strip_prefix('/') {
        let mut segments = after_slash.split('/');
        if reject_leading_opt && matches!(segments.clone().next(), Some("opt")) {
            return Err(TextError::BareOpt);
        }
        if segments.any(|segment| segment == "..") {
            return Err(TextError::ParentTraversal);
        }
    }
    Ok(())
}
