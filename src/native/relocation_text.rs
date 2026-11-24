//! Bounded relocation of Homebrew placeholders in supported UTF-8 text files.
//!
//! Homebrew relocatable bottles can carry `@@HOMEBREW_CELLAR@@` and
//! `@@HOMEBREW_PREFIX@@` placeholders in text as well as in Mach-O load
//! commands. [`rewrite`] supports:
//!
//! * `bin/pcre2-config`: preserves the exact audited handler for upstream
//!   `pcre2-config` (single-quoted variables and inert `*SunOS*`/`*BSD*` `libR`
//!   branches).
//! * `lib/pkgconfig/*.pc` and `share/pkgconfig/*.pc`: pkg-config assignments,
//!   path flags (`Libs*`, `Cflags*` including `-L`/`-I` space quoting), and
//!   comments carrying pinned Homebrew placeholders.
//! * Any other valid UTF-8 text file (without NUL or unsupported control
//!   bytes, such as CMake targets, `*-config` scripts, and `.inc` files):
//!   bounded line-by-line placeholder rewriting backed by [`Paths`].
//!
//! Nothing is rewritten by blanket substring replacement: every `@@HOMEBREW_`
//! occurrence must resolve to the managed prefix, the package itself, or a
//! pinned direct dependency receipt, with no `..` traversal and no residual
//! markers. A file with no `@@HOMEBREW_` marker returns `Ok(None)`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::relocation_paths::Paths;

/// Every Homebrew text placeholder starts with this prefix.
const MARKER: &str = "@@HOMEBREW_";
/// Bare managed-prefix placeholder.
const PREFIX_MARKER: &str = "@@HOMEBREW_PREFIX@@";
/// Marker for a complete `Cellar/<name>/<version>` path and its members.
const CELLAR_MARKER: &str = "@@HOMEBREW_CELLAR@@/";
/// Marker for an `opt/<name>` path and its members.
const OPT_MARKER: &str = "@@HOMEBREW_PREFIX@@/opt/";
/// Fixed Cellar path used by upstream `pcre2-config` in non-macOS branches.
const FIXED_CELLAR: &str = "/opt/homebrew/Cellar/";
/// Largest text file this module will inspect.
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

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

/// Which pcre2-config expansion a known `echo` line performs.
#[derive(Clone, Copy)]
enum EchoKind {
    Prefix,
    ExecPrefix,
    Cflags,
    CflagsPosix,
    LibsPosix,
    Libs8,
    Libs16,
    Libs32,
}

/// Exact upstream `echo` lines and their quoted replacements.
///
/// Only the expansion is quoted; every other byte of the line is preserved.
/// No-space output is byte-for-byte unchanged.
const ECHO_REWRITES: &[(&str, &str, EchoKind)] = &[
    (
        "      echo $prefix\n",
        "      echo \"$prefix\"\n",
        EchoKind::Prefix,
    ),
    (
        "      echo $exec_prefix\n",
        "      echo \"$exec_prefix\"\n",
        EchoKind::ExecPrefix,
    ),
    (
        "      echo $includes \n",
        "      echo \"$includes\" \n",
        EchoKind::Cflags,
    ),
    (
        "        echo $includes -DPCRE2POSIX_SHARED\n",
        "        echo \"$includes\" -DPCRE2POSIX_SHARED\n",
        EchoKind::CflagsPosix,
    ),
    (
        "        echo $libS$libR -lpcre2-posix -lpcre2-8\n",
        "        echo \"$libS$libR\" -lpcre2-posix -lpcre2-8\n",
        EchoKind::LibsPosix,
    ),
    (
        "        echo $libS$libR -lpcre2-8\n",
        "        echo \"$libS$libR\" -lpcre2-8\n",
        EchoKind::Libs8,
    ),
    (
        "        echo $libS$libR -lpcre2-16\n",
        "        echo \"$libS$libR\" -lpcre2-16\n",
        EchoKind::Libs16,
    ),
    (
        "        echo $libS$libR -lpcre2-32\n",
        "        echo \"$libS$libR\" -lpcre2-32\n",
        EchoKind::Libs32,
    ),
];

/// Exact upstream `test` lines whose possibly space-bearing operand must be
/// quoted, or the `if` never assigns `includes`.
const TEST_REWRITES: &[(&str, &str)] = &[
    (
        "      if test ${prefix}/include != /usr/include ; then\n",
        "      if test \"${prefix}/include\" != /usr/include ; then\n",
    ),
    (
        "        if test ${prefix}/include != /usr/include ; then\n",
        "        if test \"${prefix}/include\" != /usr/include ; then\n",
    ),
];

/// Comparisons that must never survive with an unquoted prefix operand.
const UNSAFE_TESTS: &[&str] = &["test ${prefix}/include"];

/// Expansions that must never survive unquoted in pcre2-config.
const UNSAFE_ECHOES: &[&str] = &[
    "echo $prefix",
    "echo $exec_prefix",
    "echo $includes",
    "echo $libS",
    "echo $libs",
];

/// The three supported text relocation lanes.
enum Kind {
    PkgConfig,
    Pcre2Config,
    GeneralText,
}

impl Kind {
    fn classify(relative: &Path) -> Result<Self> {
        let Some(text) = relative.to_str() else {
            bail!("Text relocation requires a UTF-8 relative path");
        };
        if text == "bin/pcre2-config" {
            return Ok(Self::Pcre2Config);
        }
        if let Some(name) = text
            .strip_prefix("lib/pkgconfig/")
            .or_else(|| text.strip_prefix("share/pkgconfig/"))
        {
            if !name.is_empty()
                && name.len() > ".pc".len()
                && name.ends_with(".pc")
                && !name.contains('/')
                && !name.contains("..")
                && name.bytes().all(pc_name_byte)
            {
                return Ok(Self::PkgConfig);
            }
            bail!("Unsupported pkg-config file name {name:?}");
        }
        Ok(Self::GeneralText)
    }

    fn require_trailing_newline(&self) -> bool {
        matches!(self, Self::PkgConfig | Self::Pcre2Config)
    }
}

/// Rewrite one staged text file, or return `None` when it carries no marker.
///
/// `relative` is the path below the keg root (for example
/// `lib/pkgconfig/libpcre2-8.pc`). The returned bytes are a complete
/// replacement file; the caller owns staging, verification, and writing.
pub(super) fn rewrite(
    relative: &std::path::Path,
    bytes: &[u8],
    paths: &super::relocation_paths::Paths<'_>,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !contains(bytes, MARKER.as_bytes()) {
        return Ok(None);
    }
    if bytes.len() > MAX_TEXT_BYTES {
        bail!(
            "Refusing to relocate {}: text file exceeds the {MAX_TEXT_BYTES} byte limit",
            relative.display()
        );
    }
    let kind = Kind::classify(relative)?;
    let text = decode(bytes, relative, kind.require_trailing_newline())?;
    let rewritten = match kind {
        Kind::PkgConfig => rewrite_pkg_config(text, paths)
            .with_context(|| format!("Refusing to rewrite {}", relative.display()))?,
        Kind::Pcre2Config => rewrite_pcre2_config(text, paths)
            .with_context(|| format!("Refusing to rewrite {}", relative.display()))?,
        Kind::GeneralText => rewrite_general_text(text, paths)
            .with_context(|| format!("Refusing to rewrite {}", relative.display()))?,
    };
    if rewritten.contains(MARKER) {
        bail!(
            "Refusing to leave a Homebrew marker in {}",
            relative.display()
        );
    }
    Ok(Some(rewritten.into_bytes()))
}

/// Decode a supported text file with a bounded UTF-8 byte grammar.
fn decode<'a>(bytes: &'a [u8], relative: &Path, require_trailing_newline: bool) -> Result<&'a str> {
    let text = std::str::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8 text", relative.display()))?;
    if require_trailing_newline && !text.ends_with('\n') {
        bail!(
            "Refusing to rewrite {}: file must end with a newline",
            relative.display()
        );
    }
    if text.bytes().any(|byte| {
        byte == b'\r' || (byte < 0x20 && byte != b'\n' && byte != b'\t') || byte == 0x7f
    }) {
        bail!(
            "Refusing to rewrite {}: file contains unsupported control bytes",
            relative.display()
        );
    }
    Ok(text)
}

/// Rewrite all Homebrew placeholders in pkg-config files (`lib/pkgconfig/*.pc`
/// and `share/pkgconfig/*.pc`), preserving existing quoting rules.
fn rewrite_pkg_config(text: &str, paths: &Paths<'_>) -> Result<String> {
    let mut assignments = HashMap::new();
    let mut seen = HashSet::new();
    let mut replacements = 0usize;
    let mut lines = Vec::new();

    for line in text.split_inclusive('\n') {
        if line.contains(FIXED_CELLAR) || line.contains("/opt/homebrew/") {
            bail!("Fixed /opt/homebrew path in a pkg-config file is unsupported");
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
                    bail!("duplicate pkg-config variable {name:?} carries a Homebrew placeholder");
                }
                if value.is_empty()
                    || value.chars().any(char::is_whitespace)
                    || value
                        .bytes()
                        .any(|byte| matches!(byte, b'"' | b'\'' | b'`' | b'\\' | b'#'))
                {
                    bail!("Unsupported literal path {value:?} in pkg-config variable {name:?}");
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
        bail!("Homebrew marker did not occur in a supported pkg-config context");
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

/// Rewrite one general UTF-8 text file line-by-line through [`rewrite_markers_in_line`].
fn rewrite_general_text(text: &str, paths: &Paths<'_>) -> Result<String> {
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
fn rewrite_markers_in_line(line: &str, paths: &Paths<'_>) -> Result<String> {
    let mut out = String::with_capacity(line.len() + 64);
    let mut rest = line;
    while let Some(pos) = rest.find(MARKER) {
        out.push_str(&rest[..pos]);
        let after_marker = &rest[pos..];
        if let Some(tail) = after_marker.strip_prefix(CELLAR_MARKER) {
            let (name, after_name) = take_safe_ident(tail)
                .with_context(|| format!("Missing formula name after {CELLAR_MARKER}"))?;
            let after_slash = after_name.strip_prefix('/').with_context(|| {
                format!("Missing '/' after formula name in {CELLAR_MARKER}{name}")
            })?;
            let (version, after_ver) = take_safe_ident(after_slash)
                .with_context(|| format!("Missing version after {CELLAR_MARKER}{name}/"))?;
            validate_suffix_after_keg(after_ver, false, after_marker)?;
            let placeholder = format!("{CELLAR_MARKER}{name}/{version}");
            let mapped = map_keg(paths, &placeholder)?;
            out.push_str(&mapped);
            rest = after_ver;
        } else if let Some(tail) = after_marker.strip_prefix(OPT_MARKER) {
            let (name, after_name) = take_safe_ident(tail)
                .with_context(|| format!("Missing formula name after {OPT_MARKER}"))?;
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
            bail!(
                "Unsupported Homebrew placeholder {:?}",
                &after_marker[..token_end]
            );
        }
    }
    out.push_str(rest);
    if out.contains(MARKER) {
        bail!("Rewritten line still contains an unresolved Homebrew placeholder");
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
fn validate_suffix_after_keg(suffix: &str, reject_leading_opt: bool, context: &str) -> Result<()> {
    let Some(first) = suffix.bytes().next() else {
        return Ok(());
    };
    if first != b'/' && !is_token_delimiter(first) {
        let preview_len = context.len().min(64);
        bail!(
            "Unsupported trailing token after Homebrew placeholder in {:?}",
            &context[..preview_len]
        );
    }
    let token_end = suffix
        .bytes()
        .position(is_token_delimiter)
        .unwrap_or(suffix.len());
    let path_tail = &suffix[..token_end];
    if let Some(after_slash) = path_tail.strip_prefix('/') {
        let mut segments = after_slash.split('/');
        if reject_leading_opt && matches!(segments.clone().next(), Some("opt")) {
            bail!("Bare @@HOMEBREW_PREFIX@@/opt placeholder without formula name is unsupported");
        }
        if segments.any(|segment| segment == "..") {
            bail!("Unsupported '..' traversal after Homebrew placeholder");
        }
    }
    Ok(())
}

/// Rewrite literal marker paths inside a pkg-config `Libs*`/`Cflags*` line,
/// quoting any `-L`/`-I` token whose mapped path introduces spaces.
fn rewrite_pc_field_markers(line: &str, paths: &Paths<'_>) -> Result<String> {
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
fn parse_pc_assignment(line: &str) -> Result<(&str, &str, &str)> {
    let body = line
        .strip_suffix('\n')
        .context("pkg-config line does not end with a newline")?;
    let Some((raw_lhs, raw_rhs)) = body.split_once('=') else {
        bail!("Homebrew placeholder outside a pkg-config variable assignment");
    };
    let name = raw_lhs.trim_end_matches(' ');
    if !valid_pc_name(name) {
        bail!("Unsupported pkg-config variable name {name:?}");
    }
    let rhs = raw_rhs.trim_start_matches(' ');
    let lhs_eq_ws = &body[..body.len() - rhs.len()];
    Ok((lhs_eq_ws, name, rhs))
}

/// A pkg-config value is literal, but it must remain a single safe line.
fn validate_pc_mapped_value(value: &str, allow_dollar: bool) -> Result<()> {
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
        bail!("Mapped pkg-config path {value:?} cannot be represented literally");
    }
    Ok(())
}

/// Rewrite the upstream pcre2-config script without touching unknown lines.
fn rewrite_pcre2_config(text: &str, paths: &Paths<'_>) -> Result<String> {
    if !text.starts_with("#!/bin/sh\n") {
        bail!("pcre2-config must start with the upstream #!/bin/sh line");
    }
    if !text.is_ascii() {
        bail!("pcre2-config contains non-ASCII bytes");
    }

    let mut out = String::with_capacity(text.len() + 256);
    let mut state = ConfigState::default();

    for line in text.split_inclusive('\n') {
        if contains(line.as_bytes(), MARKER.as_bytes()) {
            out.push_str(&rewrite_config_marker_line(line, paths, &mut state)?);
        } else if line.contains("/opt/homebrew/") {
            out.push_str(&rewrite_config_fixed_line(line, paths, &mut state)?);
        } else if let Some(replacement) = known_test(line) {
            state.cflags_test = true;
            out.push_str(replacement);
        } else if let Some((replacement, kind)) = known_echo(line) {
            state.mark_echo(kind)?;
            out.push_str(replacement);
        } else {
            if UNSAFE_TESTS.iter().any(|needle| line.contains(needle)) {
                bail!("Unsupported pcre2-config test line {line:?}");
            }
            if UNSAFE_ECHOES.iter().any(|needle| line.contains(needle)) {
                bail!("Unsupported pcre2-config echo line {line:?}");
            }
            out.push_str(line);
        }
    }

    state.validate()?;
    if out.contains("/opt/homebrew/") {
        bail!("pcre2-config still contains a fixed /opt/homebrew path");
    }
    Ok(out)
}

/// The marker-bearing lines pcre2-config is allowed to contain.
fn rewrite_config_marker_line(
    line: &str,
    paths: &Paths<'_>,
    state: &mut ConfigState,
) -> Result<String> {
    let body = line
        .strip_suffix('\n')
        .context("pcre2-config line does not end with a newline")?;

    if let Some(value) = body.strip_prefix("prefix=") {
        if state.prefix_assignment {
            bail!("duplicate pcre2-config prefix assignment");
        }
        let mapped = map_complete_keg_root(paths, value)?;
        state.prefix_assignment = true;
        return Ok(format!("prefix={}\n", shell_quote(&mapped)?));
    }

    if let Some(value) = body
        .strip_prefix("case \"")
        .and_then(|value| value.strip_suffix("\" in"))
    {
        if state.case_word {
            bail!("duplicate pcre2-config libS case word");
        }
        let mapped = map_keg(
            paths,
            checked_literal(value, "pcre2-config libS case word")?,
        )?;
        state.case_word = true;
        return Ok(format!("case {} in\n", shell_quote(&mapped)?));
    }

    if let Some(value) = body
        .strip_prefix("  libS=\"-L")
        .and_then(|value| value.strip_suffix('"'))
    {
        if state.libs_assignment {
            bail!("duplicate pcre2-config libS assignment");
        }
        let mapped = map_keg(
            paths,
            checked_literal(value, "pcre2-config libS assignment")?,
        )?;
        state.libs_assignment = true;
        return Ok(format!("  libS={}\n", shell_quote(&format!("-L{mapped}"))?));
    }

    bail!("Unsupported Homebrew placeholder in pcre2-config line {body:?}");
}

/// The two fixed `libR` lines pcre2-config is allowed to contain.
fn rewrite_config_fixed_line(
    line: &str,
    paths: &Paths<'_>,
    state: &mut ConfigState,
) -> Result<String> {
    let body = line
        .strip_suffix('\n')
        .context("pcre2-config line does not end with a newline")?;

    if let Some(value) = body
        .strip_prefix("  libR=\" -R")
        .and_then(|value| value.strip_suffix('"'))
    {
        if state.sunos_lib_r {
            bail!("duplicate pcre2-config *SunOS* libR assignment");
        }
        let mapped = map_fixed_cellar(paths, value)?;
        state.sunos_lib_r = true;
        return Ok(format!(
            "  libR={}\n",
            shell_quote(&format!(" -R{mapped}"))?
        ));
    }

    if let Some(value) = body
        .strip_prefix("  libR=\" -Wl,-R")
        .and_then(|value| value.strip_suffix('"'))
    {
        if state.bsd_lib_r {
            bail!("duplicate pcre2-config *BSD* libR assignment");
        }
        let mapped = map_fixed_cellar(paths, value)?;
        state.bsd_lib_r = true;
        return Ok(format!(
            "  libR={}\n",
            shell_quote(&format!(" -Wl,-R{mapped}"))?
        ));
    }

    bail!("Unsupported fixed /opt/homebrew path in pcre2-config line {body:?}");
}

/// Reject whitespace or quotes in a marker value before mapping it.
fn checked_literal<'a>(value: &'a str, context: &str) -> Result<&'a str> {
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
fn map_keg(paths: &Paths<'_>, placeholder: &str) -> Result<String> {
    match paths.keg_path(placeholder)? {
        Some(mapped) => {
            validate_mapped(&mapped)?;
            Ok(mapped)
        }
        None => bail!("Unsupported Homebrew placeholder path {placeholder:?}"),
    }
}

/// Map a complete `Cellar/<name>/<version>` or `opt/<name>` keg root.
fn map_complete_keg_root(paths: &Paths<'_>, value: &str) -> Result<String> {
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
fn map_fixed_cellar(paths: &Paths<'_>, value: &str) -> Result<String> {
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
fn validate_mapped(value: &str) -> Result<()> {
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
fn shell_quote(value: &str) -> Result<String> {
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

/// Match one audited upstream `test ${prefix}/include` line.
fn known_test(line: &str) -> Option<&'static str> {
    TEST_REWRITES
        .iter()
        .find_map(|(old, new)| (*old == line).then_some(*new))
}

/// Match one audited upstream `echo` line.
fn known_echo(line: &str) -> Option<(&'static str, EchoKind)> {
    ECHO_REWRITES
        .iter()
        .find_map(|(old, new, kind)| (*old == line).then_some((*new, *kind)))
}

/// Required and duplicated pcre2-config lines observed so far.
#[derive(Default)]
struct ConfigState {
    prefix_assignment: bool,
    case_word: bool,
    libs_assignment: bool,
    prefix_echo: bool,
    exec_prefix_echo: bool,
    cflags_test: bool,
    cflags_echo: bool,
    cflags_posix_echo: bool,
    libs_posix_echo: bool,
    libs8_echo: bool,
    libs16_echo: bool,
    libs32_echo: bool,
    sunos_lib_r: bool,
    bsd_lib_r: bool,
}

impl ConfigState {
    fn mark_echo(&mut self, kind: EchoKind) -> Result<()> {
        let slot = match kind {
            EchoKind::Prefix => &mut self.prefix_echo,
            EchoKind::ExecPrefix => &mut self.exec_prefix_echo,
            EchoKind::Cflags => &mut self.cflags_echo,
            EchoKind::CflagsPosix => &mut self.cflags_posix_echo,
            EchoKind::LibsPosix => &mut self.libs_posix_echo,
            EchoKind::Libs8 => &mut self.libs8_echo,
            EchoKind::Libs16 => &mut self.libs16_echo,
            EchoKind::Libs32 => &mut self.libs32_echo,
        };
        if *slot {
            bail!("duplicate pcre2-config echo line");
        }
        *slot = true;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if !self.prefix_assignment {
            bail!("pcre2-config is missing its pinned prefix assignment");
        }
        if !self.case_word {
            bail!("pcre2-config is missing its libS case word");
        }
        if !self.libs_assignment {
            bail!("pcre2-config is missing its libS assignment");
        }
        if !self.prefix_echo {
            bail!("pcre2-config is missing its --prefix echo");
        }
        if !self.exec_prefix_echo {
            bail!("pcre2-config is missing its --exec-prefix echo");
        }
        if !self.cflags_test {
            bail!("pcre2-config is missing its --cflags test");
        }
        if !self.cflags_echo {
            bail!("pcre2-config is missing its --cflags echo");
        }
        if !self.libs8_echo {
            bail!("pcre2-config is missing its --libs8 echo");
        }
        if self.sunos_lib_r != self.bsd_lib_r {
            bail!("pcre2-config has only one of its two fixed-prefix libR branches");
        }
        Ok(())
    }
}

/// Whether a byte slice contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Conservative bytes accepted in a pkg-config file name.
fn pc_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+' | b'@')
}

#[cfg(test)]
mod tests;
