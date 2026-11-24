//! Typed refusals of the portable upstream catalog.
//!
//! Every `Display` text is the exact message the catalog has always produced.

use super::super::validate::UrlError;

/// Why the bundled catalog text was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CatalogError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error("Invalid or duplicate portable package name or alias: {name:?}")]
    InvalidName { name: String },
    #[error("Invalid version for portable package {package}")]
    InvalidVersion { package: String },
    #[error("Portable package {package} requires artifacts and executables")]
    MissingArtifacts { package: String },
    #[error("Invalid or duplicate path {path:?} for {package}")]
    InvalidPath { path: String, package: String },
    #[error("Invalid {arch} portable artifact for {package}")]
    InvalidArtifact { arch: String, package: String },
    #[error(transparent)]
    Url(#[from] UrlError),
}

/// Why a portable catalog lookup or plan failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PortableError {
    #[error("Invalid bundled portable catalog: {0}")]
    InvalidCatalog(&'static CatalogError),
    #[error("No reviewed upstream build for '{target}'.\n  Available upstream builds: {available}")]
    NoUpstreamBuild { target: String, available: String },
    #[error("Upstream releases require a supported macOS target")]
    UnsupportedTarget,
    #[error("Portable releases require a macOS target")]
    NotMacos,
    #[error("No reviewed upstream archive for {package} on {arch}")]
    NoArchive { package: String, arch: String },
    #[error("No matching package and version in the portable catalog")]
    NoMatchingPackage,
    #[error("No reviewed upstream artifact matches this plan")]
    NoMatchingArtifact,
}
