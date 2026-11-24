//! `lib/pkgconfig/*.pc` relocation: assignments, path flags and comments.

use std::collections::{HashMap, HashSet};

use super::super::paths::Paths;
use super::error::TextError;
use super::general::rewrite_markers_in_line;
use super::{FIXED_CELLAR, MARKER, contains};

/// pkg-config variables whose values are install paths. Only these variables
/// may carry a placeholder; a marker in any other line is an unknown shape.
const PC_PATH_VARIABLES: &[&str] = &[
    "prefix",
    "exec_prefix",
    "libdir",
    "includedir",
    "bindir",
    "sbindir",
    "libexecdir",
    "datadir",
    "datarootdir",
    "sysconfdir",
    "sharedstatedir",
    "localstatedir",
    "runstatedir",
    "infodir",
    "mandir",
    "docdir",
    "dvidir",
    "htmldir",
    "pdfdir",
    "psdir",
];

/// Rewrite all Homebrew placeholders in pkg-config files (`lib/pkgconfig/*.pc`
/// and `share/pkgconfig/*.pc`), preserving existing quoting rules.
pub(super) fn rewrite_pkg_config(text: &str, paths: &Paths<'_>) -> Result<String, TextError> {
    let mut assignments = HashMap::new();
    let mut seen = HashSet::new();
    let mut replacements = 0usize;
    let mut lines = Vec::new();

    for line in text.split_inclusive('\n') {
        if line.contains(FIXED_CELLAR) || line.contains("/opt/homebrew/") {
            return Err(TextError::PcFixedPath);
        }
        if contains(line.as_bytes(), MARKER.as_bytes()) {
            if line.trim_start().starts_with('#') {
                lines.push(rewrite_markers_in_line(line, paths)?);
                replacements += 1;
            } else if is_pc_field_line(line) {
                lines.push(rewrite_pc_field_markers(line, paths)?);
                replacements += 1;
            } else {
                let (lhs_eq_ws, name, value) = parse_pc_assignment(line)?;
                if !seen.insert(name.to_owned()) {
                    return Err(TextError::PcDuplicateVariable {
                        name: name.to_owned(),
                    });
                }
                if value.is_empty()
                    || value.chars().any(char::is_whitespace)
                    || value
                        .bytes()
                        .any(|byte| matches!(byte, b'"' | b'\'' | b'`' | b'\\' | b'#'))
                {
                    return Err(TextError::PcUnsupportedLiteral {
                        value: value.to_owned(),
                        name: name.to_owned(),
                    });
                }
                let allow_dollar = value.contains('$');
                let mapped = rewrite_markers_in_line(value, paths)?;
                validate_pc_mapped_value(&mapped, allow_dollar)?;
                assignments.insert(name.to_owned(), mapped.clone());
                lines.push(format!("{lhs_eq_ws}{mapped}\n"));
                replacements += 1;
            }
        } else {
            if let Some((name, value)) = plain_pc_assignment(line) {
                assignments.insert(name.to_owned(), value.to_owned());
            }
            lines.push(line.to_owned());
        }
    }

    if replacements == 0 {
        return Err(TextError::PcNoMarkerContext);
    }

    let space_variables = space_bearing_variables(&assignments, &seen);
    let mut out = String::with_capacity(text.len() + 128);
    for line in lines {
        if is_pc_field_line(&line) {
            out.push_str(&quote_pc_field_line(&line, &space_variables));
        } else {
            out.push_str(&line);
        }
    }
    Ok(out)
}

/// Rewrite literal marker paths inside a pkg-config `Libs*`/`Cflags*` line,
/// quoting any `-L`/`-I` token whose mapped path introduces spaces.
fn rewrite_pc_field_markers(line: &str, paths: &Paths<'_>) -> Result<String, TextError> {
    let (body, has_newline) = match line.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (line, false),
    };
    let mut out = String::with_capacity(line.len() + 32);
    let mut first = true;
    for token in body.split(' ') {
        if !first {
            out.push(' ');
        }
        first = false;
        if contains(token.as_bytes(), MARKER.as_bytes()) {
            let rewritten = rewrite_markers_in_line(token, paths)?;
            if let Some(quoted) = quote_literal_pc_flag(&rewritten) {
                out.push_str(&quoted);
            } else {
                out.push_str(&rewritten);
            }
        } else {
            out.push_str(token);
        }
    }
    if has_newline {
        out.push('\n');
    }
    Ok(out)
}

fn quote_literal_pc_flag(token: &str) -> Option<String> {
    let (flag, rest) = if let Some(rest) = token.strip_prefix("-L") {
        ("-L", rest)
    } else {
        ("-I", token.strip_prefix("-I")?)
    };
    if rest.is_empty()
        || !rest.contains(' ')
        || rest.starts_with('"')
        || rest.starts_with('\'')
        || rest.contains('"')
        || rest.contains('\'')
        || rest.contains('\\')
    {
        return None;
    }
    Some(format!("{flag}\"{rest}\""))
}

/// Read an ordinary variable assignment without rewriting it.
fn plain_pc_assignment(line: &str) -> Option<(&str, &str)> {
    let body = line.strip_suffix('\n')?;
    let (lhs, rhs) = body.split_once('=')?;
    let name = lhs.trim_end_matches(' ');
    valid_pc_name(name).then_some((name, rhs.trim_start_matches(' ')))
}

/// Whether a pkg-config variable name has the accepted shape.
fn valid_pc_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
}

/// Variables whose expanded value contains a space.
fn space_bearing_variables(
    assignments: &HashMap<String, String>,
    rewritten_vars: &HashSet<String>,
) -> HashSet<String> {
    assignments
        .iter()
        .filter(|(name, value)| {
            (PC_PATH_VARIABLES.contains(&name.as_str()) || rewritten_vars.contains(name.as_str()))
                && expand_pc_value(value, assignments, 0).is_some_and(|value| value.contains(' '))
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Expand `${name}` references with a bounded recursion depth.
fn expand_pc_value(
    value: &str,
    assignments: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    if depth > 16 {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        out.push_str(&rest[..start]);
        let replacement = assignments.get(&after[..end])?;
        out.push_str(&expand_pc_value(replacement, assignments, depth + 1)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Whether a line is one of the pkg-config fields that carries path flags.
fn is_pc_field_line(line: &str) -> bool {
    ["Libs:", "Libs.private:", "Cflags:", "Cflags.private:"]
        .iter()
        .any(|field| {
            line.strip_prefix(field)
                .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with('\n'))
        })
}

/// Quote path flags whose referenced variable expands with a space.
fn quote_pc_field_line(line: &str, space_variables: &HashSet<String>) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut first = true;
    for token in line.split(' ') {
        if !first {
            out.push(' ');
        }
        first = false;
        match quote_pc_token(token, space_variables) {
            Some(quoted) => out.push_str(&quoted),
            None => out.push_str(token),
        }
    }
    out
}

/// Quote one `-L`/`-I` token when one of its variables expands with a space.
fn quote_pc_token(token: &str, space_variables: &HashSet<String>) -> Option<String> {
    let (token, newline) = match token.strip_suffix('\n') {
        Some(token) => (token, true),
        None => (token, false),
    };
    let (flag, rest) = if let Some(rest) = token.strip_prefix("-L") {
        ("-L", rest)
    } else {
        let rest = token.strip_prefix("-I")?;
        ("-I", rest)
    };
    if rest.is_empty()
        || rest.starts_with('"')
        || rest.starts_with('\'')
        || rest.contains('"')
        || rest.contains('\'')
        || rest.contains('\\')
    {
        return None;
    }

    let mut referenced = false;
    let mut space = false;
    let mut scan = rest;
    while let Some(start) = scan.find("${") {
        let after = &scan[start + 2..];
        let end = after.find('}')?;
        referenced = true;
        if space_variables.contains(&after[..end]) {
            space = true;
        }
        scan = &after[end + 1..];
    }
    if !referenced || !space {
        return None;
    }

    let mut quoted = format!("{flag}\"{rest}\"");
    if newline {
        quoted.push('\n');
    }
    Some(quoted)
}

/// Split one `lhs = rhs` pkg-config line, preserving `=` and surrounding spaces.
fn parse_pc_assignment(line: &str) -> Result<(&str, &str, &str), TextError> {
    let body = line.strip_suffix('\n').ok_or(TextError::PcMissingNewline)?;
    let Some((raw_lhs, raw_rhs)) = body.split_once('=') else {
        return Err(TextError::PcOutsideAssignment);
    };
    let name = raw_lhs.trim_end_matches(' ');
    if !valid_pc_name(name) {
        return Err(TextError::PcUnsupportedName {
            name: name.to_owned(),
        });
    }
    let rhs = raw_rhs.trim_start_matches(' ');
    let lhs_eq_ws = &body[..body.len() - rhs.len()];
    Ok((lhs_eq_ws, name, rhs))
}

/// A pkg-config value is literal, but it must remain a single safe line.
fn validate_pc_mapped_value(value: &str, allow_dollar: bool) -> Result<(), TextError> {
    if value.is_empty()
        || !value.starts_with('/')
        || value.starts_with(' ')
        || value.ends_with(' ')
        || value.bytes().any(|byte| {
            byte < 0x20
                || byte == 0x7f
                || matches!(byte, b'"' | b'\'' | b'`' | b'\\' | b'#')
                || (!allow_dollar && byte == b'$')
        })
    {
        return Err(TextError::PcUnrepresentable {
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Conservative bytes accepted in a pkg-config file name.
pub(super) fn pc_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+' | b'@')
}
