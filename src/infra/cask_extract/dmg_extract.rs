//! `.dmg` extraction through a read-only `hdiutil` mount.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::error::CaskExtractError;
use super::tree::validate_archive_entry_path;
use crate::domain::cask_plan::{CaskSourcePath, PlannedCask, PlannedCaskArtifact};

/// Mount a DMG read-only with `/usr/bin/hdiutil`, copy only declared artifacts,
/// and guarantee detach on exit via RAII.
pub(super) fn extract_dmg(
    dmg_path: &Path,
    stage_dir: &Path,
    unpack_dir: &Path,
    planned: &PlannedCask,
) -> Result<(), CaskExtractError> {
    let sources = collect_dmg_sources(planned)?;
    let mount_parent = tempfile::Builder::new()
        .prefix("dmg-mount-")
        .tempdir_in(stage_dir)
        .map_err(|source| CaskExtractError::Stream {
            action: "create DMG mount parent directory",
            source,
        })?;
    let mounted = MountedDmg::attach(dmg_path, mount_parent.path())?;

    for rel_source in sources {
        copy_from_dmg(mounted.mount_point(), unpack_dir, &rel_source)?;
    }

    mounted.detach()?;
    Ok(())
}

fn collect_dmg_sources(planned: &PlannedCask) -> Result<Vec<String>, CaskExtractError> {
    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    for artifact in &planned.artifacts {
        match artifact {
            PlannedCaskArtifact::App { source, .. } => {
                if seen.insert(source.clone()) {
                    sources.push(source.clone());
                }
            }
            PlannedCaskArtifact::Binary {
                source: CaskSourcePath::Payload(rel),
                ..
            }
            | PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::Payload(rel),
                ..
            }
            | PlannedCaskArtifact::PrefixLink {
                source: CaskSourcePath::Payload(rel),
                ..
            } => {
                if seen.insert(rel.clone()) {
                    sources.push(rel.clone());
                }
            }
            PlannedCaskArtifact::Binary {
                source: CaskSourcePath::AppBundle { .. },
                ..
            }
            | PlannedCaskArtifact::CommandWrapper {
                executable: CaskSourcePath::AppBundle { .. },
                ..
            }
            | PlannedCaskArtifact::PrefixLink {
                source: CaskSourcePath::AppBundle { .. },
                ..
            } => {}
        }
    }
    if sources.is_empty() {
        return Err(CaskExtractError::NoDmgSources {
            token: planned.token.clone(),
        });
    }
    Ok(sources)
}

fn copy_from_dmg(
    mount_point: &Path,
    unpack_dir: &Path,
    rel_source: &str,
) -> Result<(), CaskExtractError> {
    validate_archive_entry_path(rel_source, false)?;
    // Verify no path component inside the DMG up to and including `rel_source`
    // is a symlink (e.g. a DMG `/Applications` drop symlink).
    let mut current = mount_point.to_path_buf();
    for segment in rel_source.split('/') {
        current.push(segment);
        let meta = fs::symlink_metadata(&current).map_err(|source| {
            CaskExtractError::DmgArtifactMissing {
                artifact: rel_source.to_owned(),
                path: current.clone(),
                source,
            }
        })?;
        if meta.file_type().is_symlink() {
            return Err(CaskExtractError::DmgArtifactSymlink {
                artifact: rel_source.to_owned(),
                component: current,
            });
        }
    }

    let src_path = mount_point.join(rel_source);
    let dst_path = unpack_dir.join(rel_source);
    if let Some(parent) = dst_path.parent() {
        fs::create_dir_all(parent).map_err(CaskExtractError::Fs)?;
    }

    let status = Command::new("/usr/bin/ditto")
        .arg(&src_path)
        .arg(&dst_path)
        .stdin(Stdio::null())
        .status()
        .map_err(|source| CaskExtractError::Stream {
            action: "execute /usr/bin/ditto for DMG copy",
            source,
        })?;
    if !status.success() {
        return Err(CaskExtractError::DittoCopyFailed {
            artifact: rel_source.to_owned(),
            status,
        });
    }
    Ok(())
}

struct MountedDmg {
    mount_point: PathBuf,
    detached: bool,
}

impl MountedDmg {
    fn attach(dmg_path: &Path, mount_parent: &Path) -> Result<Self, CaskExtractError> {
        let output = Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noautoopen",
                "-mountrandom",
            ])
            .arg(mount_parent)
            .arg("-plist")
            .arg(dmg_path)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| CaskExtractError::Stream {
                action: "execute /usr/bin/hdiutil attach",
                source,
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CaskExtractError::HdiutilAttachFailed {
                path: dmg_path.to_path_buf(),
                status: output.status,
                stderr: stderr.trim().to_owned(),
            });
        }
        let plist = std::str::from_utf8(&output.stdout)
            .map_err(|source| CaskExtractError::HdiutilOutputNotUtf8 { source })?;
        let mount_point = parse_hdiutil_mount_point(plist)?;

        let canonical_parent =
            fs::canonicalize(mount_parent).map_err(|source| CaskExtractError::Stream {
                action: "canonicalize DMG mount parent",
                source,
            })?;
        let canonical_mount =
            fs::canonicalize(&mount_point).map_err(|source| CaskExtractError::Io {
                action: "canonicalize DMG mount",
                path: mount_point.clone(),
                source,
            })?;
        let meta =
            fs::symlink_metadata(&canonical_mount).map_err(|source| CaskExtractError::Io {
                action: "inspect DMG mount",
                path: canonical_mount.clone(),
                source,
            })?;
        if !meta.is_dir()
            || !canonical_mount.starts_with(&canonical_parent)
            || canonical_mount == canonical_parent
        {
            let _ = detach_mount(&mount_point);
            return Err(CaskExtractError::MountOutsideParent {
                mount_point,
                parent: canonical_parent,
            });
        }

        Ok(Self {
            mount_point: canonical_mount,
            detached: false,
        })
    }

    fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    fn detach(mut self) -> Result<(), CaskExtractError> {
        self.detached = true;
        detach_mount(&self.mount_point)
    }
}

impl Drop for MountedDmg {
    fn drop(&mut self) {
        if !self.detached {
            let _ = detach_mount(&self.mount_point);
        }
    }
}

fn detach_mount(mount_point: &Path) -> Result<(), CaskExtractError> {
    let status = Command::new("/usr/bin/hdiutil")
        .args(["detach", "-quiet"])
        .arg(mount_point)
        .stdin(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            let force_status = Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force", "-quiet"])
                .arg(mount_point)
                .stdin(Stdio::null())
                .status()
                .map_err(|source| CaskExtractError::Stream {
                    action: "run /usr/bin/hdiutil detach -force",
                    source,
                })?;
            if force_status.success() {
                Ok(())
            } else {
                Err(CaskExtractError::DetachFailed {
                    mount_point: mount_point.to_path_buf(),
                })
            }
        }
    }
}

/// Extract the `<key>mount-point</key><string>...</string>` path from
/// `hdiutil attach -plist` XML output.
pub(super) fn parse_hdiutil_mount_point(plist: &str) -> Result<PathBuf, CaskExtractError> {
    let key = "<key>mount-point</key>";
    let mut search_from = 0;
    while let Some(idx) = plist[search_from..].find(key) {
        let after_key = &plist[search_from + idx + key.len()..];
        let trimmed = after_key.trim_start();
        if let Some(rest) = trimmed.strip_prefix("<string>")
            && let Some(end) = rest.find("</string>")
        {
            let decoded = decode_xml_entities(&rest[..end])?;
            let path = PathBuf::from(&decoded);
            if path.is_absolute() && !decoded.contains('\0') {
                return Ok(path);
            }
            return Err(CaskExtractError::InvalidMountPoint { value: decoded });
        }
        search_from += idx + key.len();
    }
    Err(CaskExtractError::MissingMountPoint)
}

fn decode_xml_entities(input: &str) -> Result<String, CaskExtractError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';') else {
            return Err(CaskExtractError::UnterminatedXmlEntity);
        };
        let entity = &after[..semi];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "apos" => out.push('\''),
            "quot" => out.push('"'),
            hex if hex.starts_with("#x") || hex.starts_with("#X") => {
                let code = u32::from_str_radix(&hex[2..], 16)
                    .map_err(|source| CaskExtractError::InvalidHexEntity { source })?;
                let ch = char::from_u32(code).ok_or(CaskExtractError::InvalidUnicodeScalar)?;
                out.push(ch);
            }
            dec if dec.starts_with('#') => {
                let code: u32 = dec[1..]
                    .parse()
                    .map_err(|source| CaskExtractError::InvalidDecimalEntity { source })?;
                let ch = char::from_u32(code).ok_or(CaskExtractError::InvalidUnicodeScalar)?;
                out.push(ch);
            }
            other => {
                return Err(CaskExtractError::UnsupportedXmlEntity {
                    entity: other.to_owned(),
                });
            }
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}
