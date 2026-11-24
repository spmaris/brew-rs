//! Payload format detection and naked-binary staging.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::error::CaskExtractError;
use super::model::PayloadFormat;
use super::wrapper::set_executable;
use crate::domain::cask_plan::{CaskSourcePath, ContainerHint, PlannedCask, PlannedCaskArtifact};

/// Detect the payload container format from magic bytes, explicit hints, or URL.
pub(super) fn detect_format(
    path: &Path,
    url: &str,
    hint: ContainerHint,
) -> Result<PayloadFormat, CaskExtractError> {
    match hint {
        ContainerHint::Naked => return Ok(PayloadFormat::Naked),
        ContainerHint::Zip => return Ok(PayloadFormat::Zip),
        ContainerHint::Dmg => return Ok(PayloadFormat::Dmg),
        ContainerHint::Auto => {}
    }

    let io_error = |action: &'static str| {
        move |source: std::io::Error| CaskExtractError::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    };
    let mut file = File::open(path).map_err(io_error("open downloaded cask"))?;
    let meta = file
        .metadata()
        .map_err(io_error("inspect downloaded cask"))?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(CaskExtractError::EmptyOrNotRegular {
            path: path.to_path_buf(),
        });
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
            .map_err(|source| CaskExtractError::Stream {
                action: "seek to DMG trailer",
                source,
            })?;
        let mut trailer = [0_u8; 4];
        file.read_exact(&mut trailer)
            .map_err(|source| CaskExtractError::Stream {
                action: "read DMG trailer",
                source,
            })?;
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

    Err(CaskExtractError::UnsupportedFormat {
        url_path: url_path.to_owned(),
    })
}

pub(super) fn read_prefix(file: &mut File, buf: &mut [u8]) -> Result<usize, CaskExtractError> {
    let mut read_total = 0;
    while read_total < buf.len() {
        let n = file
            .read(&mut buf[read_total..])
            .map_err(|source| CaskExtractError::Stream {
                action: "read cask header",
                source,
            })?;
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
) -> Result<(), CaskExtractError> {
    let mut payload_paths = HashSet::new();
    for artifact in &planned.artifacts {
        match artifact {
            PlannedCaskArtifact::App { bundle_name, .. } => {
                return Err(CaskExtractError::NakedAppBundle {
                    bundle_name: bundle_name.clone(),
                });
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
                return Err(CaskExtractError::NakedAppdirReference);
            }
        }
    }
    let payload_count = payload_paths.len();
    let mut payload_paths = payload_paths.into_iter();
    let (Some(rel), None) = (payload_paths.next(), payload_paths.next()) else {
        return Err(CaskExtractError::NakedPayloadCount {
            count: payload_count,
        });
    };
    let dest = unpack_dir.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(CaskExtractError::io("create directory", parent))?;
    }
    fs::copy(download_path, &dest).map_err(|source| CaskExtractError::Io {
        action: "stage naked cask binary to",
        path: dest.clone(),
        source,
    })?;
    set_executable(&dest)?;
    Ok(())
}
