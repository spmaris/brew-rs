//! Payload format detection and naked-binary staging.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::model::PayloadFormat;
use super::wrapper::set_executable;
use crate::domain::cask_plan::{CaskSourcePath, ContainerHint, PlannedCask, PlannedCaskArtifact};

/// Detect the payload container format from magic bytes, explicit hints, or URL.
pub(super) fn detect_format(path: &Path, url: &str, hint: ContainerHint) -> Result<PayloadFormat> {
    match hint {
        ContainerHint::Naked => return Ok(PayloadFormat::Naked),
        ContainerHint::Zip => return Ok(PayloadFormat::Zip),
        ContainerHint::Dmg => return Ok(PayloadFormat::Dmg),
        ContainerHint::Auto => {}
    }

    let mut file = File::open(path)
        .with_context(|| format!("failed to open downloaded cask '{}'", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to inspect downloaded cask '{}'", path.display()))?;
    if !meta.is_file() || meta.len() == 0 {
        bail!(
            "downloaded cask '{}' is empty or not a regular file",
            path.display()
        );
    }

    let mut head = [0_u8; 16];
    let head_len = read_prefix(&mut file, &mut head)?;
    let head = &head[..head_len];

    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(PayloadFormat::Zip);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return Ok(PayloadFormat::TarGz);
    }
    if head.starts_with(b"BZh") {
        return Ok(PayloadFormat::TarBz2);
    }
    if head.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        return Ok(PayloadFormat::TarXz);
    }

    // UDIF DMG images end with a 512-byte `koly` trailer.
    if meta.len() >= 512 {
        file.seek(SeekFrom::End(-512))
            .context("failed to seek to DMG trailer")?;
        let mut trailer = [0_u8; 4];
        file.read_exact(&mut trailer)
            .context("failed to read DMG trailer")?;
        if &trailer == b"koly" {
            return Ok(PayloadFormat::Dmg);
        }
    }

    if is_macho_or_script_magic(head) {
        return Ok(PayloadFormat::Naked);
    }

    let url_path = url.split(['?', '#']).next().unwrap_or(url);
    let lower_path = url_path.to_ascii_lowercase();
    if lower_path.ends_with(".dmg") {
        return Ok(PayloadFormat::Dmg);
    }
    if lower_path.ends_with(".zip") {
        return Ok(PayloadFormat::Zip);
    }
    if lower_path.ends_with(".tar.gz") || lower_path.ends_with(".tgz") {
        return Ok(PayloadFormat::TarGz);
    }

    bail!("unrecognized cask payload format for '{url_path}'")
}

pub(super) fn read_prefix(file: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut read_total = 0;
    while read_total < buf.len() {
        let n = file
            .read(&mut buf[read_total..])
            .context("failed to read cask header")?;
        if n == 0 {
            break;
        }
        read_total += n;
    }
    Ok(read_total)
}

pub(super) fn is_macho_or_script_magic(head: &[u8]) -> bool {
    if head.starts_with(b"#!") {
        return true;
    }
    if head.len() < 4 {
        return false;
    }
    matches!(
        &head[..4],
        [0xCF, 0xFA, 0xED, 0xFE]
            | [0xFE, 0xED, 0xFA, 0xCF]
            | [0xCE, 0xFA, 0xED, 0xFE]
            | [0xFE, 0xED, 0xFA, 0xCE]
            | [0xCA, 0xFE, 0xBA, 0xBE]
            | [0xBE, 0xBA, 0xFE, 0xCA]
    )
}

pub(super) fn stage_naked_payload(
    download_path: &Path,
    unpack_dir: &Path,
    planned: &PlannedCask,
) -> Result<()> {
    let mut payload_paths = HashSet::new();
    for artifact in &planned.artifacts {
        match artifact {
            PlannedCaskArtifact::App { bundle_name, .. } => {
                bail!(
                    "cask declares app bundle '{bundle_name}' but downloaded payload is a naked file"
                );
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
                payload_paths.insert(rel.as_str());
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
            } => {
                bail!("cask references $APPDIR bundle in a naked file payload");
            }
        }
    }
    let payload_count = payload_paths.len();
    let mut payload_paths = payload_paths.into_iter();
    let (Some(rel), None) = (payload_paths.next(), payload_paths.next()) else {
        bail!("naked cask payload must map to exactly one payload file (got {payload_count})");
    };
    let dest = unpack_dir.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(download_path, &dest)
        .with_context(|| format!("failed to stage naked cask binary to '{}'", dest.display()))?;
    set_executable(&dest)?;
    Ok(())
}
