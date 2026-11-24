//! Bounded declarative model for the explicit third-party tap lane.
//!
//! A tap manifest is user-provided TOML. It is parsed as data and never
//! evaluated: there is no Ruby, no shell, no source build, and no formula
//! compatibility claim. Every identifier, URL, digest, and architecture is
//! validated before the manifest can enter brewrs state, and every collection
//! is bounded.
//!
//! The manifest format is intentionally narrow:
//!
//! ```toml
//! schema = 1
//! tap = "pproenca/tap"
//!
//! [[packages]]
//! token = "agent-tui"
//! version = "1.1.0"
//! bin = "agent-tui"
//!
//! [packages.artifacts.arm64]
//! url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-arm64"
//! sha256 = "0daed495a2d90acce7a720cd5ef94c7613c54b1277bffc7466b998f3edf023b5"
//!
//! [packages.artifacts.x86_64]
//! url = "https://github.com/pproenca/agent-tui/releases/download/v1.1.0/agent-tui-darwin-x64"
//! sha256 = "81c071afb4df3c688059ebe6336f543cfe5e79082147d3571274c2508ea798cb"
//! ```
//!
//! Unknown fields, unknown architectures, duplicate identifiers, duplicate
//! artifacts, reserved names, and unsupported schemas fail closed.

mod error;
mod limits;
mod manifest;
mod names;
mod record;
mod reserved;

pub(crate) use limits::{
    MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES, MAX_PACKAGES, MAX_RECEIPT_BYTES, MAX_RECORD_BYTES,
    MAX_TAPS, TAPROOM,
};
pub(crate) use manifest::{Artifact, ManifestPackage, TapManifest};
pub(crate) use names::{
    QualifiedTarget, host_arch, is_known_arch, parse_qualified, parse_tap_name,
};
pub(crate) use record::{TapReceipt, TapRecord};

#[cfg(test)]
mod tests;
