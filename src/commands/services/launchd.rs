use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Maximum argv elements accepted from one `service.run` declaration.
pub(super) const MAX_SERVICE_ARGS: usize = 64;

/// Maximum length of one argv element, path, or environment value.
pub(super) const MAX_SERVICE_LITERAL_LEN: usize = 4096;

/// Maximum declared environment variables.
pub(super) const MAX_SERVICE_ENV: usize = 64;

/// Maximum length of an environment variable name.
pub(super) const MAX_SERVICE_ENV_NAME_LEN: usize = 64;

/// Maximum length of a service label or declared service name.
pub(super) const MAX_SERVICE_NAME_LEN: usize = 255;

/// Maximum length of a rendered `run_type`.
pub(super) const MAX_RUN_TYPE_LEN: usize = 64;

/// Maximum length of a declared service key.
pub(super) const MAX_SERVICE_KEY_LEN: usize = 64;

/// Maximum length after placeholder expansion. Bounds the amplification of a
/// bounded literal that contains many placeholders.
pub(super) const MAX_SERVICE_EXPANDED_LEN: usize = 16 * 1024;

/// Service keys that this preview interprets for display. Every other declared
/// key is reported as unmodelled rather than silently ignored.
pub(super) const MODELLED_SERVICE_KEYS: [&str; 7] = [
    "run",
    "run_type",
    "environment_variables",
    "working_dir",
    "name",
    "log_path",
    "error_log_path",
];

/// Extract the macOS argv from a validated `service.run` declaration.
///
/// A shell command string is refused because this preview never parses a shell
/// command line; only an explicit argv array is rendered.
pub(super) fn macos_run_args(service: &Map<String, Value>) -> Result<&[Value]> {
    let run = service
        .get("run")
        .context("Service declaration has no run command")?;
    let value = match run {
        Value::Array(_) => run,
        Value::Object(os_map) => os_map
            .get("macos")
            .context("Service run declaration has no macOS entry")?,
        Value::String(_) => {
            bail!("Service run is a shell command string; this preview only models argv arrays")
        }
        _ => bail!("Service run has an unsupported shape"),
    };
    match value {
        Value::Array(args) => Ok(args),
        Value::String(_) => {
            bail!("Service run is a shell command string; this preview only models argv arrays")
        }
        _ => bail!("Service run must be an argv array"),
    }
}

pub(super) fn macos_name(value: &Value) -> Result<Option<String>> {
    let name = match value {
        Value::String(name) => Some(name),
        Value::Object(os_map) => match os_map.get("macos") {
            Some(Value::String(name)) => Some(name),
            Some(_) => bail!("Service name macOS entry must be a string"),
            None => None,
        },
        _ => bail!("Service name must be a string or an OS map"),
    };
    match name {
        None => Ok(None),
        Some(name) => {
            if name.is_empty()
                || name.len() > MAX_SERVICE_NAME_LEN
                || !name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
                })
            {
                bail!("Service name is not a bounded launchd label");
            }
            Ok(Some(name.clone()))
        }
    }
}

pub(super) fn validate_environment_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid = name.len() <= MAX_SERVICE_ENV_NAME_LEN
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
    if !valid {
        bail!("Service environment variable {name:?} is not a bounded variable name");
    }
    Ok(())
}

/// Bound one untrusted literal before it is rendered or interpreted.
pub(super) fn review_literal(value: &str, what: &str) -> Result<()> {
    if value.is_empty() {
        bail!("Service {what} must not be empty");
    }
    if value.len() > MAX_SERVICE_LITERAL_LEN {
        bail!("Service {what} is longer than the {MAX_SERVICE_LITERAL_LEN}-byte limit");
    }
    if value.chars().any(char::is_control) {
        bail!("Service {what} contains a control character");
    }
    Ok(())
}

/// Expand only the two Homebrew path placeholders modelled by this preview.
///
/// Substitution is a single left-to-right scan of the original literal: text
/// inserted from the prefix is never rescanned, so a prefix that itself
/// contains a placeholder-looking string cannot be expanded twice. An unbraced
/// placeholder must end at a token boundary, so `$HOMEBREW_PREFIX_UNKNOWN` is
/// refused instead of being read as the prefix followed by text. Any other `$`
/// reference is refused rather than resolved from the ambient environment, so
/// no environment inheritance is implied.
pub(super) fn expand_literal(prefix: &Path, value: &str, what: &str) -> Result<String> {
    review_literal(value, what)?;
    let prefix_text = displayable_path(prefix)?;
    let cellar = prefix.join("Cellar");
    let cellar_text = displayable_path(&cellar)?;
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        expanded.push_str(&rest[..index]);
        let candidate = &rest[index..];
        let (replacement, consumed) = if let Some(braced) = candidate.strip_prefix("${") {
            if braced.starts_with("HOMEBREW_PREFIX}") {
                (prefix_text, "${HOMEBREW_PREFIX}".len())
            } else if braced.starts_with("HOMEBREW_CELLAR}") {
                (cellar_text, "${HOMEBREW_CELLAR}".len())
            } else {
                bail!("{}", unsupported_variable(what, value));
            }
        } else if let Some(tail) = candidate.strip_prefix("$HOMEBREW_PREFIX") {
            if starts_variable_name(tail) {
                bail!("{}", unsupported_variable(what, value));
            }
            (prefix_text, "$HOMEBREW_PREFIX".len())
        } else if let Some(tail) = candidate.strip_prefix("$HOMEBREW_CELLAR") {
            if starts_variable_name(tail) {
                bail!("{}", unsupported_variable(what, value));
            }
            (cellar_text, "$HOMEBREW_CELLAR".len())
        } else {
            bail!("{}", unsupported_variable(what, value));
        };
        if expanded.len() + replacement.len() > MAX_SERVICE_EXPANDED_LEN {
            bail!("Service {what} expands beyond the {MAX_SERVICE_EXPANDED_LEN}-byte limit");
        }
        expanded.push_str(replacement);
        rest = &candidate[consumed..];
    }
    if expanded.len() + rest.len() > MAX_SERVICE_EXPANDED_LEN {
        bail!("Service {what} expands beyond the {MAX_SERVICE_EXPANDED_LEN}-byte limit");
    }
    expanded.push_str(rest);
    Ok(expanded)
}

pub(super) fn starts_variable_name(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
}

pub(super) fn unsupported_variable(what: &str, value: &str) -> String {
    format!(
        "Service {what} uses a variable this preview does not model (only $HOMEBREW_PREFIX and $HOMEBREW_CELLAR are expanded): {value:?}"
    )
}

/// Require an absolute, lexically normalized path with no `.`/`..` components.
pub(super) fn absolute_path(value: &str, what: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("Service {what} must be an absolute path after expansion: {value:?}");
    }
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => {
                bail!("Service {what} must not contain '.' or '..' path components: {value:?}")
            }
            _ => bail!("Service {what} has an unsupported path prefix: {value:?}"),
        }
    }
    Ok(path.to_path_buf())
}

pub(super) fn prefix_hash(prefix: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(
        displayable_path(prefix)?.as_bytes(),
    )))
}

/// Keep the reviewed Colima namespace shape: `local.brewrs.<hash>.<suffix>`.
/// Characters that launchd labels do not accept are replaced in the suffix.
pub(super) fn local_label(hash: &str, name: &str) -> String {
    let suffix: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!("local.brewrs.{}.{suffix}", &hash[..16])
}

pub(super) fn render_argv(args: &[String], offset: usize) -> String {
    args.iter()
        .enumerate()
        .map(|(index, argument)| format!("[{}] {argument:?}", index + offset))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn displayable_path(path: &Path) -> Result<&str> {
    let value = path
        .to_str()
        .context("Service preview requires UTF-8 paths")?;
    if value.contains(':') || value.chars().any(char::is_control) {
        bail!("Service preview cannot represent paths containing colons or control characters");
    }
    Ok(value)
}
