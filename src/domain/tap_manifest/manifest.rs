use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::domain::validate::{is_lower_hex_digest, validate_cask_url};

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
    pub(crate) fn parse(bytes: &[u8], expected_tap: &str) -> Result<Self> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            bail!("tap manifest exceeds the {MAX_MANIFEST_BYTES} byte limit");
        }
        let text = std::str::from_utf8(bytes).context("tap manifest is not valid UTF-8")?;
        let manifest: Self =
            toml::from_str(text).context("failed to parse the tap manifest as TOML")?;
        manifest.validate(expected_tap)?;
        Ok(manifest)
    }

    /// Validate the whole manifest, including cross-package collisions.
    fn validate(&self, expected_tap: &str) -> Result<()> {
        if self.schema != SCHEMA_VERSION {
            bail!(
                "unsupported tap manifest schema {}; expected {SCHEMA_VERSION}",
                self.schema
            );
        }
        let (owner, repo) = parse_tap_name(&self.tap)?;
        if self.tap != expected_tap {
            bail!("manifest declares tap '{}', not '{expected_tap}'", self.tap);
        }
        if self.packages.is_empty() {
            bail!("tap '{owner}/{repo}' declares no packages");
        }
        if self.packages.len() > MAX_PACKAGES {
            bail!(
                "tap '{owner}/{repo}' declares {} packages, above the {MAX_PACKAGES} package limit",
                self.packages.len()
            );
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
                bail!(
                    "tap '{owner}/{repo}' declares package token '{}' more than once",
                    package.token
                );
            }
            if !bins.insert(package.bin.as_str()) {
                bail!(
                    "tap '{owner}/{repo}' declares executable name '{}' more than once",
                    package.bin
                );
            }
            if package.artifacts.is_empty() {
                bail!(
                    "tap package '{}' declares no architecture artifacts",
                    package.token
                );
            }
            if package.artifacts.len() > 2 {
                bail!(
                    "tap package '{}' declares {} artifacts; only arm64 and x86_64 are supported",
                    package.token,
                    package.artifacts.len()
                );
            }
            for (architecture, artifact) in &package.artifacts {
                if !is_known_arch(architecture) {
                    bail!(
                        "tap package '{}' declares unsupported architecture '{architecture}'; expected arm64 or x86_64",
                        package.token
                    );
                }
                if !is_lower_hex_digest(&artifact.sha256) {
                    bail!(
                        "tap package '{}' has an invalid sha256 for '{architecture}': expected 64 lowercase hexadecimal characters",
                        package.token
                    );
                }
                if artifact.url.len() > MAX_URL_BYTES {
                    bail!(
                        "tap package '{}' has a URL longer than {MAX_URL_BYTES} bytes",
                        package.token
                    );
                }
                validate_cask_url(&artifact.url).with_context(|| {
                    format!(
                        "tap package '{}' has an unsafe '{architecture}' artifact URL",
                        package.token
                    )
                })?;
                if !urls.insert(artifact.url.as_str()) {
                    bail!(
                        "tap '{owner}/{repo}' declares artifact URL '{}' more than once; selected artifacts must be unambiguous",
                        artifact.url
                    );
                }
                if !digests.insert(artifact.sha256.as_str()) {
                    bail!(
                        "tap '{owner}/{repo}' declares artifact sha256 '{}' more than once; selected artifacts must be unambiguous",
                        artifact.sha256
                    );
                }
            }
        }

        // A token in one package must not name another package's executable.
        for (index, package) in self.packages.iter().enumerate() {
            for other in self.packages.iter().skip(index + 1) {
                if package.token == other.bin || package.bin == other.token {
                    bail!(
                        "tap '{owner}/{repo}' has colliding identifiers: package '{}' and executable '{}' name different packages",
                        package.token,
                        package.bin
                    );
                }
            }
        }
        Ok(())
    }

    /// Find one declared package by exact token.
    pub(crate) fn package(&self, token: &str) -> Result<&ManifestPackage> {
        self.packages
            .iter()
            .find(|package| package.token == token)
            .with_context(|| {
                format!(
                    "tap '{}' does not declare package '{token}'; declared packages: {}",
                    self.tap,
                    self.packages
                        .iter()
                        .map(|package| package.token.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

impl ManifestPackage {
    /// Select the one artifact for `architecture`.
    pub(crate) fn artifact_for(&self, architecture: &str) -> Result<&Artifact> {
        if let Some(artifact) = self.artifacts.get(architecture) {
            return Ok(artifact);
        }
        bail!(
            "tap package '{}' has no {architecture} artifact; declared architectures: {}",
            self.token,
            self.artifacts
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
