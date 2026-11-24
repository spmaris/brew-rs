//! Data-driven installs of reviewed, standalone macOS tar.gz releases.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::plan::{self, PlannedFormula};
use super::store::{self, PackageSource};
use super::{archive, cask_download, tap_artifact};

const BUNDLED_TOML: &str = include_str!("portable.toml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalog {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Package {
    pub name: String,
    version: String,
    #[serde(default)]
    aliases: Vec<String>,
    executables: Vec<String>,
    #[serde(default)]
    required_files: Vec<String>,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    root: String,
    url: String,
    sha256: String,
}

pub(super) fn catalog() -> Result<&'static Catalog> {
    static CATALOG: OnceLock<Result<Catalog, String>> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::parse(BUNDLED_TOML).map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow::anyhow!("Invalid bundled portable catalog: {error}"))
}

impl Catalog {
    fn parse(text: &str) -> Result<Self> {
        let catalog: Self = toml::from_str(text)?;
        let mut names = HashSet::new();
        for package in &catalog.packages {
            for name in std::iter::once(&package.name).chain(&package.aliases) {
                if name.contains('/') || plan::parse_target(name).is_err() || !names.insert(name) {
                    bail!("Invalid or duplicate portable package name or alias: {name:?}");
                }
            }
            if !store::is_safe_version(&package.version) {
                bail!("Invalid version for portable package {}", package.name);
            }
            if package.artifacts.is_empty() || package.executables.is_empty() {
                bail!(
                    "Portable package {} requires artifacts and executables",
                    package.name
                );
            }
            let mut paths = HashSet::new();
            for (path, executable) in package
                .executables
                .iter()
                .map(|path| (path, true))
                .chain(package.required_files.iter().map(|path| (path, false)))
            {
                let single_bin = path.split_once('/').is_some_and(|(dir, file)| {
                    matches!(dir, "bin" | "sbin") && !file.contains('/')
                });
                if !safe_relative(path) || (executable && !single_bin) || !paths.insert(path) {
                    bail!("Invalid or duplicate path {path:?} for {}", package.name);
                }
            }
            let mut digests = HashSet::new();
            for (arch, asset) in &package.artifacts {
                if !matches!(arch.as_str(), "arm64" | "x86_64")
                    || !safe_relative(&asset.root)
                    || asset.root.contains('/')
                    || !store::is_lower_hex_digest(&asset.sha256)
                    || !digests.insert(&asset.sha256)
                {
                    bail!("Invalid {arch} portable artifact for {}", package.name);
                }
                cask_download::validate_cask_url(&asset.url)?;
            }
        }
        Ok(catalog)
    }

    pub fn find(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|package| {
            package.name == name || package.aliases.iter().any(|alias| alias == name)
        })
    }

    /// `name (alias, alias)` for every entry, comma-separated, for error hints.
    fn available_builds(&self) -> String {
        self.packages
            .iter()
            .map(|package| {
                if package.aliases.is_empty() {
                    package.name.clone()
                } else {
                    format!("{} ({})", package.name, package.aliases.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Plan every target from the catalog for `--source upstream`.
    ///
    /// Homebrew is never consulted. Each target must name a catalog entry (by
    /// name or alias) that ships an artifact for the host architecture.
    pub fn plan_targets(&self, targets: &[String], tag: &str) -> Result<Vec<PlannedFormula>> {
        let mut plan: Vec<PlannedFormula> = Vec::new();
        for target in targets {
            let package = self.find(target).with_context(|| {
                format!(
                    "No reviewed upstream build for '{target}'.\n  Available upstream builds: {}",
                    self.available_builds()
                )
            })?;
            if !plan.iter().any(|formula| formula.name == package.name) {
                plan.push(package.plan(tag)?);
            }
        }
        Ok(plan)
    }
}

impl Package {
    /// Whether the catalog ships an artifact for the host architecture of `tag`.
    pub fn supports(&self, tag: &str) -> bool {
        plan::is_supported_macos_tag(tag)
            && plan::arch_for_tag(tag).is_some_and(|arch| self.artifacts.contains_key(arch))
    }

    pub fn plan(&self, tag: &str) -> Result<PlannedFormula> {
        if !plan::is_supported_macos_tag(tag) {
            bail!("Upstream releases require a supported macOS target");
        }
        let arch = plan::arch_for_tag(tag).context("Portable releases require a macOS target")?;
        let artifact = self
            .artifacts
            .get(arch)
            .with_context(|| format!("No reviewed upstream archive for {} on {arch}", self.name))?;
        Ok(PlannedFormula {
            name: self.name.clone(),
            version: self.version.clone(),
            version_scheme: Some(0),
            keg_only: false,
            has_service: false,
            caveats: None,
            conflicts_with: Vec::new(),
            requested: true,
            dependencies: Vec::new(),
            url: artifact.url.clone(),
            sha256: artifact.sha256.clone(),
            cellar: ":any_skip_relocation".into(),
            install_blockers: Vec::new(),
            source: PackageSource::Upstream,
        })
    }
}

fn safe_relative(path: &str) -> bool {
    path.split('/').all(|part| {
        !matches!(part, "" | "." | ".." | ".brew")
            && part.len() <= 128
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-@".contains(&b))
    }) && path.len() <= 512
}

fn artifact_for(
    formula: &PlannedFormula,
) -> Result<(&'static Package, &'static str, &'static Artifact)> {
    let package = catalog()?
        .find(&formula.name)
        .filter(|package| package.name == formula.name && package.version == formula.version)
        .context("No matching package and version in the portable catalog")?;
    package
        .artifacts
        .iter()
        .find_map(|(arch, artifact)| {
            (formula.source == PackageSource::Upstream
                && formula.url == artifact.url
                && formula.sha256 == artifact.sha256)
                .then_some((package, arch.as_str(), artifact))
        })
        .context("No reviewed upstream artifact matches this plan")
}

pub(super) async fn fetch(
    config: &crate::config::Config,
    formula: &PlannedFormula,
) -> Result<PathBuf> {
    let (_, _, artifact) = artifact_for(formula)?;
    cask_download::fetch(config, &artifact.url, &artifact.sha256).await
}

pub(super) fn stage(formula: &PlannedFormula, tarball: &Path, out: &Path) -> Result<()> {
    let (package, arch, artifact) = artifact_for(formula)?;
    archive::unpack_release(tarball, out, &artifact.root, &artifact.sha256)?;
    for relative in &package.required_files {
        regular_member(out, relative)?;
    }
    for relative in &package.executables {
        let path = regular_member(out, relative)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::symlink_metadata(&path)?.permissions().mode() & 0o111 == 0 {
                bail!("Portable archive member {relative:?} is not executable");
            }
        }
        tap_artifact::validate_macho(&path, arch)?;
    }
    Ok(())
}

fn regular_member(out: &Path, relative: &str) -> Result<PathBuf> {
    let path = out.join(relative);
    // The private stage is canonicalized independently so callers may use a
    // prefix behind macOS's /var or /tmp aliases, but members cannot be aliases.
    if !fs::symlink_metadata(&path)?.is_file()
        || path.canonicalize()? != out.canonicalize()?.join(relative)
    {
        bail!(
            "Portable archive member {relative:?} must be a regular file with real directory ancestors"
        );
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
        [[packages]]
        name = "sample"
        aliases = ["alias"]
        version = "2.0.0"
        executables = ["bin/sample"]
        required_files = ["share/sample/data"]
        [packages.artifacts.arm64]
        root = "sample-mac"
        url = "https://example.org/sample.tar.gz"
        sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    "#;

    #[test]
    fn a_second_package_only_needs_catalog_data() {
        let catalog = Catalog::parse(&format!("{BUNDLED_TOML}\n{FIXTURE}")).unwrap();
        let sample = catalog.find("alias").unwrap();
        assert_eq!(sample.plan("arm64_sequoia").unwrap().name, "sample");
        assert!(sample.plan("sequoia").is_err());
        assert!(catalog.find("homebrew/core/sample").is_none());
    }

    #[test]
    fn rejects_ambiguous_names_and_unsafe_catalog_fields() {
        for bad in [
            format!("{FIXTURE}\n{FIXTURE}"),
            FIXTURE.replace("bin/sample", "bin/../sample"),
            FIXTURE.replace("sample-mac", "../outside"),
            FIXTURE.replace("https:", "http:"),
            FIXTURE.replace("required_files", "unknown_key"),
        ] {
            assert!(Catalog::parse(&bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn binds_plan_to_exact_artifact() {
        for package in &catalog().unwrap().packages {
            for (arch, tag) in [("arm64", "arm64_sequoia"), ("x86_64", "sequoia")] {
                if !package.artifacts.contains_key(arch) {
                    continue;
                }
                let mut plan = package.plan(tag).unwrap();
                assert_eq!(artifact_for(&plan).unwrap().1, arch);
                plan.sha256.replace_range(..1, "z");
                assert!(artifact_for(&plan).is_err());
            }
        }
    }

    #[test]
    fn upstream_source_plans_catalog_entries_and_rejects_unknown_names() {
        let catalog = catalog().unwrap();
        let plan = catalog
            .plan_targets(&["nvim".into(), "neovim".into()], "arm64_sequoia")
            .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "neovim");
        assert_eq!(plan[0].source, PackageSource::Upstream);
        let error = catalog
            .plan_targets(&["nosuchpackage".into()], "arm64_sequoia")
            .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "No reviewed upstream build for 'nosuchpackage'.\n  Available upstream builds: neovim (nvim)"
        );
        assert!(
            catalog
                .plan_targets(&["homebrew/core/neovim".into()], "arm64_sequoia")
                .is_err()
        );
    }

    #[test]
    fn upstream_source_requires_an_artifact_for_the_host() {
        let catalog = catalog().unwrap();
        // The bundled neovim entry ships arm64 only.
        assert!(catalog.plan_targets(&["neovim".into()], "sequoia").is_err());
        assert!(
            catalog
                .plan_targets(&["neovim".into()], "arm64_linux")
                .is_err()
        );
        assert!(!catalog.find("neovim").unwrap().supports("sequoia"));
        assert!(catalog.find("neovim").unwrap().supports("arm64_sequoia"));
    }
}
