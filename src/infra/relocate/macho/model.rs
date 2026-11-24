//! Parsed Mach-O image model: header, load-command kinds and path categories.

use std::ops::Range;

use super::constants::{
    LC_ID_DYLIB, LC_LAZY_LOAD_DYLIB, LC_LOAD_DYLIB, LC_LOAD_UPWARD_DYLIB, LC_LOAD_WEAK_DYLIB,
    LC_REEXPORT_DYLIB, LC_RPATH,
};

/// Header fields that relocation must preserve exactly.
#[derive(Debug, Clone, Copy)]
pub(super) struct Header {
    pub(super) cputype: u32,
    pub(super) cpusubtype: u32,
    pub(super) filetype: u32,
}

/// Policy category of a Mach-O load-command path passed to the mapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::infra::relocate) enum PathKind {
    Dylib,
    IdDylib,
    Rpath,
}

/// A load command recognized as carrying a relocatable path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    LoadDylib,
    WeakDylib,
    ReexportDylib,
    UpwardDylib,
    LazyDylib,
    IdDylib,
    Rpath,
}

impl Kind {
    pub(super) fn from_command(command: u32) -> Option<Self> {
        match command {
            LC_LOAD_DYLIB => Some(Self::LoadDylib),
            LC_LOAD_WEAK_DYLIB => Some(Self::WeakDylib),
            LC_REEXPORT_DYLIB => Some(Self::ReexportDylib),
            LC_LOAD_UPWARD_DYLIB => Some(Self::UpwardDylib),
            LC_LAZY_LOAD_DYLIB => Some(Self::LazyDylib),
            LC_ID_DYLIB => Some(Self::IdDylib),
            LC_RPATH => Some(Self::Rpath),
            _ => None,
        }
    }

    pub(super) fn path_kind(self) -> PathKind {
        match self {
            Self::IdDylib => PathKind::IdDylib,
            Self::Rpath => PathKind::Rpath,
            Self::LoadDylib
            | Self::WeakDylib
            | Self::ReexportDylib
            | Self::UpwardDylib
            | Self::LazyDylib => PathKind::Dylib,
        }
    }

    pub(super) fn is_rpath(self) -> bool {
        matches!(self, Self::Rpath)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::LoadDylib => "LC_LOAD_DYLIB",
            Self::WeakDylib => "LC_LOAD_WEAK_DYLIB",
            Self::ReexportDylib => "LC_REEXPORT_DYLIB",
            Self::UpwardDylib => "LC_LOAD_UPWARD_DYLIB",
            Self::LazyDylib => "LC_LAZY_LOAD_DYLIB",
            Self::IdDylib => "LC_ID_DYLIB",
            Self::Rpath => "LC_RPATH",
        }
    }
}

/// One parsed load command.
#[derive(Debug)]
pub(super) struct LoadCommand {
    pub(super) command: u32,
    pub(super) kind: Option<Kind>,
    pub(super) name: Option<String>,
    pub(super) name_range: Option<Range<usize>>,
}

/// A structurally validated thin 64-bit Mach-O image.
#[derive(Debug)]
pub(super) struct Parsed {
    pub(super) header: Header,
    pub(super) commands: Vec<LoadCommand>,
    /// Whether the image arrived with a non-empty embedded code signature.
    pub(super) signed: bool,
}
