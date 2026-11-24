//! Plan output types and the untrusted formula API payload shapes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The `--source` choice of `install`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SourceFlag {
    /// Resolve through Homebrew (the default).
    Homebrew,
    /// Install the reviewed official upstream build from the bundled catalog.
    Upstream,
}

/// The source used for installation and retained for future upgrades.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageSource {
    #[default]
    Homebrew,
    Upstream,
}

impl PackageSource {
    /// Human-readable name used in user-facing messages.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Upstream => "upstream",
        }
    }
}

/// One formula in a native install plan.
///
/// `name` is the canonical API name, `version` is the stable version with the
/// Homebrew revision suffix when needed, and `requested` distinguishes an
/// explicit target from a dependency pulled in to satisfy one. `dependencies`
/// lists the canonical names of this formula's direct runtime dependencies in
/// API order, deduplicated, so a consumer can rely on every entry appearing
/// earlier in the plan. `url`, `sha256` and `cellar` come from the selected
/// bottle entry for the plan's host tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFormula {
    pub name: String,
    pub version: String,
    pub version_scheme: Option<u32>,
    pub keg_only: bool,
    pub has_service: bool,
    pub caveats: Option<String>,
    pub conflicts_with: Vec<String>,
    pub requested: bool,
    pub dependencies: Vec<String>,
    pub url: String,
    pub sha256: String,
    pub cellar: String,
    pub install_blockers: Vec<&'static str>,
    /// Where the artifact comes from; routing never inspects `cellar` for this.
    pub source: PackageSource,
}

/// A validated formula after the fields needed by the plan have been checked.
pub(super) struct FormulaNode {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) version_scheme: Option<u32>,
    pub(super) keg_only: bool,
    pub(super) has_service: bool,
    pub(super) caveats: Option<String>,
    pub(super) conflicts_with: Vec<String>,
    pub(super) dependencies: Vec<String>,
    pub(super) url: String,
    pub(super) sha256: String,
    pub(super) cellar: String,
    pub(super) install_blockers: Vec<&'static str>,
}

/// The subset of the official formula payload used by the planner.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct FormulaResponse {
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) tap: String,
    #[serde(default)]
    pub(super) versions: FormulaVersions,
    #[serde(default)]
    pub(super) revision: Option<u32>,
    pub(super) version_scheme: Option<u32>,
    #[serde(default)]
    pub(super) dependencies: Vec<String>,
    #[serde(default)]
    pub(super) variations: HashMap<String, Variation>,
    #[serde(default)]
    pub(super) bottle: Option<Bottle>,
    #[serde(default)]
    pub(super) disabled: bool,
    pub(super) post_install_defined: Option<bool>,
    pub(super) keg_only: Option<bool>,
    #[serde(default)]
    pub(super) caveats: Option<String>,
    #[serde(default)]
    pub(super) recommended_dependencies: Vec<String>,
    #[serde(default)]
    pub(super) uses_from_macos: Vec<serde_json::Value>,
    #[serde(default)]
    pub(super) uses_from_macos_bounds: Vec<serde_json::Value>,
    #[serde(default)]
    pub(super) conflicts_with: Vec<String>,
    #[serde(default)]
    pub(super) service: serde_json::Value,
    #[serde(default)]
    pub(super) pour_bottle_only_if: serde_json::Value,
    #[serde(default)]
    pub(super) requirements: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub(super) struct FormulaVersions {
    #[serde(default)]
    pub(super) stable: Option<String>,
    #[serde(default)]
    pub(super) bottle: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Variation {
    /// `None` means the variation does not override dependencies; `Some(vec![])`
    /// means it explicitly removes them.
    #[serde(default)]
    pub(super) dependencies: Option<Vec<String>>,
    #[serde(flatten)]
    pub(super) extra: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Bottle {
    #[serde(default)]
    pub(super) stable: Option<BottleStable>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct BottleStable {
    #[serde(default)]
    pub(super) files: HashMap<String, BottleFile>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct BottleFile {
    #[serde(default)]
    pub(super) cellar: String,
    #[serde(default)]
    pub(super) url: String,
    #[serde(default)]
    pub(super) sha256: String,
}
