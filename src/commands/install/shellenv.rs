use std::path::Path;

use anyhow::{Context, Result, bail};

use super::prefix_path;

/// Print shell environment exports for the selected brewrs prefix without
/// creating or modifying any files.
pub(crate) fn shellenv(prefix: Option<&Path>, shell: Option<&str>) -> Result<()> {
    let path = prefix_path(prefix)?;
    let script = render_shellenv(&path, shell, std::env::var_os("SHELL").as_deref())?;
    print!("{script}");
    Ok(())
}

pub(super) fn render_shellenv(
    prefix: &Path,
    shell: Option<&str>,
    shell_env: Option<&std::ffi::OsStr>,
) -> Result<String> {
    let raw = prefix
        .to_str()
        .context("brewrs prefix must be valid UTF-8 for shellenv")?;
    if raw.is_empty() {
        bail!("refusing an empty brewrs prefix");
    }
    if !prefix.is_absolute() {
        bail!(
            "refusing relative brewrs prefix '{}'; use an absolute path",
            prefix.display()
        );
    }
    if raw.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        bail!("refusing brewrs prefix with control characters for shellenv");
    }
    let normalized = if raw.len() > 1 {
        raw.trim_end_matches('/')
    } else {
        raw
    };
    let normalized = if normalized.is_empty() {
        "/"
    } else {
        normalized
    };
    let bin = if normalized == "/" {
        "/bin".to_string()
    } else {
        format!("{normalized}/bin")
    };
    let sbin = if normalized == "/" {
        "/sbin".to_string()
    } else {
        format!("{normalized}/sbin")
    };

    let is_fish = match shell {
        Some("fish") => true,
        Some(_) => false,
        None => {
            shell_env
                .and_then(|value| Path::new(value).file_name())
                .and_then(|name| name.to_str())
                == Some("fish")
        }
    };

    if is_fish {
        Ok(format!(
            "set -gx BREWRS_PREFIX {};\nfish_add_path -gP {} {};\n",
            fish_quote(normalized),
            fish_quote(&bin),
            fish_quote(&sbin),
        ))
    } else {
        let path_entries = format!("{bin}:{sbin}");
        Ok(format!(
            "export BREWRS_PREFIX={};\nexport PATH={}:\"$PATH\";\n",
            posix_quote(normalized),
            posix_quote(&path_entries),
        ))
    }
}

pub(super) fn posix_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub(super) fn fish_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}
