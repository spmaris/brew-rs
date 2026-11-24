//! Command-wrapper script rendering and executable-bit handling.

use std::fs;
use std::path::Path;

use super::error::CaskExtractError;

pub(super) fn build_command_wrapper_script(executable: &Path, args: &[String]) -> String {
    let mut script = String::from("#!/bin/sh\nexec ");
    script.push_str(&shell_quote(&executable.to_string_lossy()));
    for arg in args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push_str(" \"$@\"\n");
    script
}

pub(super) fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

pub(super) fn set_executable(path: &Path) -> Result<(), CaskExtractError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|source| {
            CaskExtractError::Io {
                action: "chmod 755",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
