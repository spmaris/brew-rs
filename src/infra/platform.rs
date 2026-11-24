//! Host platform detection.

use anyhow::{Context, Result, bail};

use crate::domain::plan::macos_tag;

/// The Homebrew bottle tag for the machine brewrs runs on.
pub(crate) fn default_target() -> Result<String> {
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
                .context("Could not detect macOS version; pass --target")?;
            if !version.status.success() {
                bail!("Could not detect macOS version; pass --target");
            }
            let version = std::str::from_utf8(&version.stdout).context("Invalid macOS version")?;
            macos_tag(arch, version.trim())
        }
        _ => bail!("No default Homebrew bottle tag for this platform; pass --target"),
    }
}
