mod archive;
pub(crate) mod deps;
mod download;
pub(crate) mod info;
pub(crate) mod install;
mod plan;
mod store;
mod version;

use anyhow::{Context, Result, bail};
use std::io::{self, Write};

use crate::{config::Config, ui::DnfStyleFormatter};

pub async fn run(
    config: &Config,
    packages: &[String],
    target: Option<&str>,
    fetch: bool,
) -> Result<()> {
    let tag = match target {
        Some(tag) => tag.to_owned(),
        None => default_target()?,
    };
    let plan = plan::resolve(config, packages, &tag).await?;
    let ui = DnfStyleFormatter::new(config);
    let rows: Vec<_> = plan
        .iter()
        .map(|package| {
            vec![
                package.name.clone(),
                package.version.clone(),
                if package.requested {
                    "Requested"
                } else {
                    "Dependency"
                }
                .into(),
                if package.cellar == ":any_skip_relocation" {
                    "Portable"
                } else if package.cellar == ":any" {
                    "Relocation needed"
                } else {
                    "Fixed prefix"
                }
                .into(),
            ]
        })
        .collect();
    println!(
        "{}  {}",
        ui.section(if fetch {
            "Bottle fetch"
        } else {
            "Install plan"
        }),
        ui.info(&tag)
    );
    println!(
        "{}",
        ui.table(&["Package", "Version", "Reason", "Bottle"], &rows)
    );
    if !fetch {
        println!("\n{} formulae planned. No packages changed.", plan.len());
        return Ok(());
    }

    io::stdout().flush()?;
    for (index, package) in plan.iter().enumerate() {
        let path = download::fetch(config, &package.url, &package.sha256)
            .await
            .with_context(|| {
                format!(
                    "Could not fetch the bottle for {} {}",
                    package.name, package.version
                )
            })?;
        if log::log_enabled!(log::Level::Info) {
            log::info!("Verified bottle: {}", path.display());
        }
        println!(
            "[{}/{}] {}  {} {}",
            index + 1,
            plan.len(),
            ui.success("Verified"),
            package.name,
            package.version
        );
    }
    println!("\nVerified {} bottles. No packages installed.", plan.len());
    Ok(())
}

fn default_target() -> Result<String> {
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

fn macos_tag(arch: &str, version: &str) -> Result<String> {
    let release = match version.split('.').next() {
        Some("11") => "big_sur",
        Some("12") => "monterey",
        Some("13") => "ventura",
        Some("14") => "sonoma",
        Some("15") => "sequoia",
        Some("26") => "tahoe",
        _ => bail!("Unknown macOS bottle tag for {version}; pass --target"),
    };
    match arch {
        "aarch64" => Ok(format!("arm64_{release}")),
        "x86_64" => Ok(release.into()),
        _ => bail!("No default Homebrew bottle tag for {arch}; pass --target"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_platform_keys_match_homebrew() {
        assert_eq!(macos_tag("aarch64", "15.7.1").unwrap(), "arm64_sequoia");
        assert_eq!(macos_tag("x86_64", "15.7").unwrap(), "sequoia");
        assert_eq!(macos_tag("aarch64", "26.1").unwrap(), "arm64_tahoe");
        assert!(macos_tag("aarch64", "27.0").is_err());
    }
}
