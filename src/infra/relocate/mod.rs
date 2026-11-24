//! Apply the supported relocation lane to verified, private staged kegs.
//!
//! Rewritten paths are absolute and version-pinned, not `opt` aliases. A path
//! may name the staging root of an exact receipt in the same batch or an
//! existing managed keg. Only staging files are ever rewritten. The caller
//! must discard the batch on any error and let the store recheck receipts
//! under its writer lock before publishing it.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use crate::infra::store::Prepared;
use macho::PathKind;
use paths::{Paths, RpathTarget};

mod error;
mod macho;
mod paths;
mod text;

pub(crate) use error::RelocateError;

/// Largest non-Mach-O file read into memory for a text rewrite.
const MAX_RELOCATION_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// Largest Mach-O image read into memory for relocation.
const MAX_MACHO_RELOCATION_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Chunk size for scanning oversized non-Mach-O files.
const SCAN_CHUNK_BYTES: usize = 1024 * 1024;

/// A staged file after classification.
enum Loaded {
    Macho(Vec<u8>),
    Text(Vec<u8>),
    /// Oversized and free of relocation markers: nothing to rewrite.
    Skip,
}

/// Size limits and scan chunk used by [`load`]; injectable for tests.
#[derive(Clone, Copy)]
struct Limits {
    text: u64,
    macho: u64,
    chunk: usize,
}

const LIMITS: Limits = Limits {
    text: MAX_RELOCATION_FILE_BYTES,
    macho: MAX_MACHO_RELOCATION_BYTES,
    chunk: SCAN_CHUNK_BYTES,
};

/// Classify `path` from a bounded prefix and its size, then read it as far as
/// its class requires. Only Mach-O images may exceed the text limit.
fn load(path: &Path, limits: Limits) -> Result<Loaded, RelocateError> {
    let mut file = File::open(path).map_err(RelocateError::io("read", path))?;
    let len = file
        .metadata()
        .map_err(RelocateError::io("inspect", path))?
        .len();
    let mut prefix = Vec::new();
    (&mut file)
        .take(macho::MACHO_PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)
        .map_err(RelocateError::io("read", path))?;
    let is_macho = macho::is_macho_prefix(&prefix, len);
    let limit = if is_macho { limits.macho } else { limits.text };
    if len > limit {
        if is_macho {
            return Err(RelocateError::FileTooLarge { limit });
        }
        file.rewind().map_err(RelocateError::io("read", path))?;
        let marked =
            text::stream_has_needle(file, limits.chunk).map_err(RelocateError::io("read", path))?;
        return if marked {
            Err(RelocateError::FileTooLarge { limit })
        } else {
            Ok(Loaded::Skip)
        };
    }
    file.rewind().map_err(RelocateError::io("read", path))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(RelocateError::io("read", path))?;
    if bytes.len() as u64 > limit {
        return Err(RelocateError::FileTooLarge { limit });
    }
    Ok(if is_macho {
        Loaded::Macho(bytes)
    } else {
        Loaded::Text(bytes)
    })
}

pub(crate) struct Item<'a> {
    pub(crate) package: &'a Prepared,
    pub(crate) needs_relocation: bool,
}

pub(crate) fn preflight() -> Result<(), RelocateError> {
    if !cfg!(target_os = "macos") {
        return Err(RelocateError::MacosRequired);
    }
    for tool in ["/usr/bin/install_name_tool", "/usr/bin/codesign"] {
        if !Path::new(tool).is_file() {
            return Err(RelocateError::ToolMissing { tool });
        }
    }
    Ok(())
}

pub(crate) fn staged<'a>(
    prefix: &Path,
    items: impl IntoIterator<Item = Item<'a>>,
) -> Result<(), RelocateError> {
    let items: Vec<_> = items.into_iter().collect();
    if !items.iter().any(|item| item.needs_relocation) {
        return Ok(());
    }
    let prefix = prefix
        .canonicalize()
        .map_err(|source| RelocateError::PrefixResolve { source })?;
    let mut batch = HashMap::new();
    for item in &items {
        let pkg = item.package;
        if batch
            .insert(
                (pkg.receipt.name.as_str(), pkg.receipt.version.as_str()),
                pkg.stage.path(),
            )
            .is_some()
        {
            return Err(RelocateError::DuplicateStage {
                name: pkg.receipt.name.clone(),
                version: pkg.receipt.version.clone(),
            });
        }
    }
    for item in items.iter().filter(|item| item.needs_relocation) {
        let pkg = item.package;
        let paths = Paths::new(&prefix, &pkg.receipt)?;
        relocate_keg(pkg.stage.path(), &prefix, &paths, &batch).map_err(|source| {
            RelocateError::Keg {
                name: pkg.receipt.name.clone(),
                version: pkg.receipt.version.clone(),
                source: Box::new(source),
            }
        })?;
    }
    Ok(())
}

fn relocate_keg(
    stage: &Path,
    prefix: &Path,
    paths: &Paths<'_>,
    batch: &HashMap<(&str, &str), &Path>,
) -> Result<(), RelocateError> {
    let mut pending = vec![stage.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
            .map_err(RelocateError::io("read directory", &directory))?;
        entries.sort_unstable_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(RelocateError::io("inspect", &path))?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                rewrite_file(stage, &path, prefix, paths, batch).map_err(|source| {
                    RelocateError::StagedFile {
                        path: path.clone(),
                        source: Box::new(source),
                    }
                })?;
            } else if !kind.is_symlink() {
                return Err(RelocateError::UnexpectedEntry { path });
            }
            // The extractor has already checked keg symlinks. Never follow
            // them while walking or rewrite the same physical file twice.
        }
    }
    Ok(())
}

fn rewrite_file(
    stage: &Path,
    path: &Path,
    prefix: &Path,
    paths: &Paths<'_>,
    batch: &HashMap<(&str, &str), &Path>,
) -> Result<(), RelocateError> {
    let loaded = load(path, LIMITS)?;
    let relative = path.strip_prefix(stage)?;
    if let Loaded::Macho(bytes) = &loaded {
        macho::relocate(path, bytes, |kind, old, rpaths| match kind {
            PathKind::Rpath => match paths.rpath_path(old, relative)? {
                RpathTarget::KegDir(mapped) => {
                    validate_target(prefix, Path::new(&mapped), batch, TargetKind::Dir)?;
                    Ok(Some(mapped))
                }
                RpathTarget::PrefixLib(mapped) => Ok(Some(mapped)),
            },
            PathKind::Dylib => {
                if let Some(member) = Paths::rpath_member(old)? {
                    let resolves = rpaths.iter().any(|raw_rpath| {
                        let Ok(RpathTarget::KegDir(dir)) = paths.rpath_path(raw_rpath, relative)
                        else {
                            return false;
                        };
                        let candidate = Path::new(&dir).join(member);
                        validate_target(prefix, &candidate, batch, TargetKind::File).is_ok()
                    });
                    if !resolves {
                        return Err(RelocateError::RpathUnresolved {
                            old: old.to_owned(),
                            image: relative.to_path_buf(),
                        });
                    }
                    return Ok(None);
                }
                let mapped = paths.macho_path(old, relative)?;
                if let Some(mapped) = &mapped {
                    validate_target(prefix, Path::new(mapped), batch, TargetKind::File)?;
                }
                Ok(mapped)
            }
            PathKind::IdDylib => {
                let mapped = paths.macho_path(old, relative)?;
                if let Some(mapped) = &mapped {
                    validate_target(prefix, Path::new(mapped), batch, TargetKind::File)?;
                }
                Ok(mapped)
            }
        })?;
    } else if let Loaded::Text(bytes) = &loaded
        && let Some(rewritten) = text::rewrite(relative, bytes, paths)?
    {
        fs::write(path, rewritten).map_err(RelocateError::io("write", path))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum TargetKind {
    File,
    Dir,
}

fn validate_target(
    prefix: &Path,
    target: &Path,
    batch: &HashMap<(&str, &str), &Path>,
    kind: TargetKind,
) -> Result<(), RelocateError> {
    let (name, version, member) = target
        .strip_prefix(prefix.join("Cellar"))?
        .to_str()
        .and_then(|s| {
            let (name, tail) = s.split_once('/')?;
            if name.is_empty() || tail.is_empty() {
                return None;
            }
            match tail.split_once('/') {
                Some((version, member)) => Some((name, version, Some(member))),
                None => Some((name, tail, None)),
            }
        })
        .ok_or(RelocateError::NotPinnedKeg)?;
    if matches!(kind, TargetKind::File) && member.is_none_or(str::is_empty) {
        return Err(RelocateError::NotFileInKeg);
    }
    let installed_keg: PathBuf;
    let root = match batch.get(&(name, version)) {
        Some(stage) => *stage,
        None => {
            let cellar = prefix.join("Cellar");
            let package = cellar.join(name);
            installed_keg = package.join(version);
            for path in [&cellar, &package, &installed_keg] {
                if !fs::symlink_metadata(path)
                    .map_err(RelocateError::io("inspect", path))?
                    .is_dir()
                {
                    return Err(RelocateError::TargetNotManagedKeg {
                        target: target.to_path_buf(),
                    });
                }
            }
            &installed_keg
        }
    };
    let root = root
        .canonicalize()
        .map_err(RelocateError::io("resolve", root))?;
    let candidate = match member {
        Some(member) if !member.is_empty() => root.join(member),
        _ => root.clone(),
    };
    let real = candidate
        .canonicalize()
        .map_err(|source| RelocateError::TargetMissing {
            target: target.to_path_buf(),
            source,
        })?;
    let type_ok = match kind {
        TargetKind::File => real.is_file(),
        TargetKind::Dir => real.is_dir(),
    };
    if !real.starts_with(&root) || !type_ok {
        let kind = match kind {
            TargetKind::File => "regular file",
            TargetKind::Dir => "directory",
        };
        return Err(RelocateError::TargetWrongKind {
            kind,
            target: target.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
