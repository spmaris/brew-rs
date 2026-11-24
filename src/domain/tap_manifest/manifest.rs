use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::domain::validate::{is_lower_hex_digest, validate_cask_url};

use super::error::ManifestError;
use super::limits::{MAX_MANIFEST_BYTES, MAX_PACKAGES, MAX_URL_BYTES, SCHEMA_VERSION};
use super::names::{
    is_known_arch, parse_tap_name, validate_bin_identifier, validate_token_identifier,
    validate_version,
};

/// One local declarative tap manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TapManifest {
    pub(super) schema: u32,
    pub(super) tap: String,
    pub(crate) packages: Vec<ManifestPackage>,
}

/// One package declared by a tap manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestPackage {
    pub(crate) token: String,
    pub(crate) version: String,
    pub(crate) bin: String,
    pub(crate) artifacts: BTreeMap<String, Artifact>,
}

/// One architecture-specific naked executable artifact.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub(crate) url: String,
    pub(crate) sha256: String,
}

impl TapManifest {
    /// Parse and validate a manifest for exactly `expected_tap`.
    pub(crate) fn parse(bytes: &[u8], expected_tap: &str) -> Result<Self, ManifestError> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge);
        }
        let text = std::str::from_utf8(bytes).map_err(ManifestError::NotUtf8)?;
        let manifest: Self = toml::from_str(text).map_err(ManifestError::InvalidToml)?;
        manifest.validate(expected_tap)?;
        Ok(manifest)
    }

    /// Validate the whole manifest, including cross-package collisions.
    fn validate(&self, expected_tap: &str) -> Result<(), ManifestError> {
        if self.schema != SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema { found: self.schema });
        }
        // The validated name is exactly `<owner>/<repo>`, so it names the tap.
        parse_tap_name(&self.tap)?;
        let tap = self.tap.as_str();
        if self.tap != expected_tap {
            return Err(ManifestError::TapMismatch {
                declared: self.tap.clone(),
                expected: expected_tap.to_owned(),
            });
        }
        if self.packages.is_empty() {
            return Err(ManifestError::NoPackages {
                tap: tap.to_owned(),
            });
        }
        if self.packages.len() > MAX_PACKAGES {
            return Err(ManifestError::TooManyPackages {
                tap: tap.to_owned(),
                count: self.packages.len(),
            });
        }

        let mut tokens = HashSet::with_capacity(self.packages.len());
        let mut bins = HashSet::with_capacity(self.packages.len());
        let mut urls = HashSet::new();
        let mut digests = HashSet::new();

        for package in &self.packages {
            validate_token_identifier(&package.token)?;
            validate_version(&package.version)?;
            validate_bin_identifier(&package.bin)?;
            if !tokens.insert(package.token.as_str()) {
                return Err(ManifestError::DuplicateToken {
                    tap: tap.to_owned(),
                    token: package.token.clone(),
                });
            }
            if !bins.insert(package.bin.as_str()) {
                return Err(ManifestError::DuplicateBin {
                    tap: tap.to_owned(),
                    bin: package.bin.clone(),
                });
            }
            if package.artifacts.is_empty() {
                return Err(ManifestError::NoArtifacts {
                    token: package.token.clone(),
                });
            }
            if package.artifacts.len() > 2 {
                return Err(ManifestError::TooManyArtifacts {
                    token: package.token.clone(),
                    count: package.artifacts.len(),
                });
            }
            for (architecture, artifact) in &package.artifacts {
                if !is_known_arch(architecture) {
                    return Err(ManifestError::UnsupportedArchitecture {
                        token: package.token.clone(),
                        architecture: architecture.clone(),
                    });
                }
                if !is_lower_hex_digest(&artifact.sha256) {
                    return Err(ManifestError::InvalidSha256 {
                        token: package.token.clone(),
                        architecture: architecture.clone(),
                    });
                }
                if artifact.url.len() > MAX_URL_BYTES {
                    return Err(ManifestError::UrlTooLong {
                        token: package.token.clone(),
                    });
                }
                validate_cask_url(&artifact.url).map_err(|source| ManifestError::UnsafeUrl {
                    token: package.token.clone(),
                    architecture: architecture.clone(),
                    source,
                })?;
                if !urls.insert(artifact.url.as_str()) {
                    return Err(ManifestError::DuplicateUrl {
                        tap: tap.to_owned(),
                        url: artifact.url.clone(),
                    });
                }
                if !digests.insert(artifact.sha256.as_str()) {
                    return Err(ManifestError::DuplicateSha256 {
                        tap: tap.to_owned(),
                        sha256: artifact.sha256.clone(),
                    });
                }
            }
        }

        // A token in one package must not name another package's executable.
        for (index, package) in self.packages.iter().enumerate() {
            for other in self.packages.iter().skip(index + 1) {
                if package.token == other.bin || package.bin == other.token {
                    return Err(ManifestError::CollidingIdentifiers {
                        tap: tap.to_owned(),
                        token: package.token.clone(),
                        bin: package.bin.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Find one declared package by exact token.
    pub(crate) fn package(&self, token: &str) -> Result<&ManifestPackage, ManifestError> {
        self.packages
            .iter()
            .find(|package| package.token == token)
            .ok_or_else(|| ManifestError::UndeclaredPackage {
                tap: self.tap.clone(),
                token: token.to_owned(),
                declared: self
                    .packages
                    .iter()
                    .map(|package| package.token.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }
}

impl ManifestPackage {
    /// Select the one artifact for `architecture`.
    pub(crate) fn artifact_for(&self, architecture: &str) -> Result<&Artifact, ManifestError> {
        if let Some(artifact) = self.artifacts.get(architecture) {
            return Ok(artifact);
        }
        Err(ManifestError::NoArtifactForArchitecture {
            token: self.token.clone(),
            architecture: architecture.to_owned(),
            declared: self
                .artifacts
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        })
    }
}
