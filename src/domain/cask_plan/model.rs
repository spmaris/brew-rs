//! Planned cask output types.

/// Container extraction hint declared by a cask payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerHint {
    Auto,
    Naked,
    Zip,
    Dmg,
}

/// Location of a cask source file inside either the unpacked archive or a
/// declared `.app` bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaskSourcePath {
    Payload(String),
    AppBundle {
        bundle_name: String,
        inner_path: String,
    },
}

/// One validated cask artifact supported by native cask installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlannedCaskArtifact {
    App {
        source: String,
        bundle_name: String,
    },
    Binary {
        source: CaskSourcePath,
        link_name: String,
    },
    CommandWrapper {
        executable: CaskSourcePath,
        link_name: String,
        args: Vec<String>,
    },
    PrefixLink {
        source: CaskSourcePath,
        relative_dest: String,
    },
}

/// Planned cask installation metadata and any blockers preventing install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedCask {
    pub token: String,
    pub name: Vec<String>,
    pub version: String,
    pub auto_updates: bool,
    pub url: String,
    pub sha256: String,
    pub container: ContainerHint,
    pub artifacts: Vec<PlannedCaskArtifact>,
    pub conflicts_with_casks: Vec<String>,
    pub conflicts_with_formulae: Vec<String>,
    pub install_blockers: Vec<String>,
}
