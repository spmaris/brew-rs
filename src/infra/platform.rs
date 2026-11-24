//! Host platform detection.

use std::io;
use std::process::ExitStatus;
use std::str::Utf8Error;

use crate::domain::plan::macos_tag;

/// Why the host's default bottle tag could not be determined.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PlatformError {
    #[error("Could not detect macOS version; pass --target")]
    SwVersSpawn {
        #[source]
        source: io::Error,
    },
    /// `sw_vers` ran but exited unsuccessfully; the status and captured output
    /// are kept for callers even though the message does not repeat them.
    #[error("Could not detect macOS version; pass --target")]
    SwVersFailed {
        #[allow(dead_code)]
        status: ExitStatus,
        #[allow(dead_code)]
        output: String,
    },
    #[error("Invalid macOS version")]
    SwVersNotUtf8 {
        #[source]
        source: Utf8Error,
    },
    #[error(transparent)]
    Tag(Box<dyn std::error::Error + Send + Sync>),
    #[error("No default Homebrew bottle tag for this platform; pass --target")]
    Unsupported,
}

/// The Homebrew bottle tag for the machine brewrs runs on.
pub(crate) fn default_target() -> Result<String, PlatformError> {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "linux" if matches!(arch, "aarch64" | "x86_64") => Ok(if arch == "aarch64" {
            "arm64_linux"
        } else {
            "x86_64_linux"
        }
        .into()),
        "macos" => {
            let version = std::process::Command::new("/usr/bin/sw_vers")
                .arg("-productVersion")
                .output()
                .map_err(|source| PlatformError::SwVersSpawn { source })?;
            if !version.status.success() {
                return Err(PlatformError::SwVersFailed {
                    status: version.status,
                    output: String::from_utf8_lossy(&version.stdout).into_owned(),
                });
            }
            let version = std::str::from_utf8(&version.stdout)
                .map_err(|source| PlatformError::SwVersNotUtf8 { source })?;
            macos_tag(arch, version.trim()).map_err(|error| PlatformError::Tag(error.into()))
        }
        _ => Err(PlatformError::Unsupported),
    }
}
