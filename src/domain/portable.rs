//! Data-driven installs of reviewed, standalone macOS tar.gz releases.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use serde::Deserialize;

use super::plan::{self, PackageSource, PlannedFormula};
use super::validate::{is_lower_hex_digest, is_safe_version, validate_cask_url};

mod error;

pub(crate) use error::{CatalogError, PortableError};

const BUNDLED_TOML: &str = include_str!("portable.toml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Package {
    pub name: String,
    pub version: String,
    #[serde(default)]
    aliases: Vec<String>,
    pub executables: Vec<String>,
    #[serde(default)]
    pub required_files: Vec<String>,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub root: String,
    pub url: String,
    pub sha256: String,
}

pub(crate) fn catalog() -> Result<&'static Catalog, PortableError> {
    static CATALOG: OnceLock<Result<Catalog, CatalogError>> = OnceLock::new();
    CATALOG
        .get_or_init(|| Catalog::parse(BUNDLED_TOML))
        .as_ref()
        .map_err(PortableError::InvalidCatalog)
}

impl Catalog {
    fn parse(text: &str) -> Result<Self, CatalogError> {
        let catalog: Self = toml::from_str(text)?;
        let mut names = HashSet::new();
        for package in &catalog.packages {
            for name in std::iter::once(&package.name).chain(&package.aliases) {
                if name.contains('/') || plan::parse_target(name).is_err() || !names.insert(name) {
                    return Err(CatalogError::InvalidName { name: name.clone() });
                }
            }
            if !is_safe_version(&package.version) {
                return Err(CatalogError::InvalidVersion {
                    package: package.name.clone(),
                });
            }
            if package.artifacts.is_empty() || package.executables.is_empty() {
                return Err(CatalogError::MissingArtifacts {
                    package: package.name.clone(),
                });
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
                    return Err(CatalogError::InvalidPath {
                        path: path.clone(),
                        package: package.name.clone(),
                    });
                }
            }
            let mut digests = HashSet::new();
            for (arch, asset) in &package.artifacts {
                if !matches!(arch.as_str(), "arm64" | "x86_64")
                    || !safe_relative(&asset.root)
                    || asset.root.contains('/')
                    || !is_lower_hex_digest(&asset.sha256)
                    || !digests.insert(&asset.sha256)
                {
                    return Err(CatalogError::InvalidArtifact {
                        arch: arch.clone(),
                        package: package.name.clone(),
                    });
                }
                validate_cask_url(&asset.url)?;
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
    pub fn plan_targets(
        &self,
        targets: &[String],
        tag: &str,
    ) -> Result<Vec<PlannedFormula>, PortableError> {
        let mut plan: Vec<PlannedFormula> = Vec::new();
        for target in targets {
            let package = self
                .find(target)
                .ok_or_else(|| PortableError::NoUpstreamBuild {
                    target: target.clone(),
                    available: self.available_builds(),
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

    pub fn plan(&self, tag: &str) -> Result<PlannedFormula, PortableError> {
        if !plan::is_supported_macos_tag(tag) {
            return Err(PortableError::UnsupportedTarget);
        }
        let arch = plan::arch_for_tag(tag).ok_or(PortableError::NotMacos)?;
        let artifact = self
            .artifacts
            .get(arch)
            .ok_or_else(|| PortableError::NoArchive {
                package: self.name.clone(),
                arch: arch.to_owned(),
            })?;
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

/// The catalog entry, host architecture and artifact a plan was built from.
pub(crate) fn artifact_for(
    formula: &PlannedFormula,
) -> Result<(&'static Package, &'static str, &'static Artifact), PortableError> {
    let package = catalog()?
        .find(&formula.name)
        .filter(|package| package.name == formula.name && package.version == formula.version)
        .ok_or(PortableError::NoMatchingPackage)?;
    package
        .artifacts
        .iter()
        .find_map(|(arch, artifact)| {
            (formula.source == PackageSource::Upstream
                && formula.url == artifact.url
                && formula.sha256 == artifact.sha256)
                .then_some((package, arch.as_str(), artifact))
        })
        .ok_or(PortableError::NoMatchingArtifact)
}

#[cfg(test)]
mod tests;
