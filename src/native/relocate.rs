//! Apply the supported relocation lane to verified, private staged kegs.
//!
//! Rewritten paths are absolute and version-pinned, not `opt` aliases. A path
//! may name the staging root of an exact receipt in the same batch or an
//! existing managed keg. Only staging files are ever rewritten. The caller
//! must discard the batch on any error and let the store recheck receipts
//! under its writer lock before publishing it.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::macho_relocate::{self, PathKind};
use super::relocation_paths::{Paths, RpathTarget};
use super::relocation_text;
use super::store::Prepared;

const MAX_RELOCATION_FILE_BYTES: u64 = 128 * 1024 * 1024;

pub(super) struct Item<'a> {
    pub(super) package: &'a Prepared,
    pub(super) needs_relocation: bool,
}

pub(super) fn preflight() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("Bottle relocation requires macOS");
    }
    for tool in ["/usr/bin/install_name_tool", "/usr/bin/codesign"] {
        if !Path::new(tool).is_file() {
            bail!(
                "Bottle relocation requires {tool}; install the Apple Command Line Tools and retry"
            );
        }
    }
    Ok(())
}

pub(super) fn staged<'a>(prefix: &Path, items: impl IntoIterator<Item = Item<'a>>) -> Result<()> {
    let items: Vec<_> = items.into_iter().collect();
    if !items.iter().any(|item| item.needs_relocation) {
        return Ok(());
    }
    let prefix = prefix
        .canonicalize()
        .context("Could not resolve the relocation prefix")?;
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
            bail!(
                "Duplicate staged keg for {} {}",
                pkg.receipt.name,
                pkg.receipt.version
            );
        }
    }
    for item in items.iter().filter(|item| item.needs_relocation) {
        let pkg = item.package;
        let paths = Paths::new(&prefix, &pkg.receipt)?;
        relocate_keg(pkg.stage.path(), &prefix, &paths, &batch).with_context(|| {
            format!(
                "Could not relocate {} {}",
                pkg.receipt.name, pkg.receipt.version
            )
        })?;
    }
    Ok(())
}

fn relocate_keg(
    stage: &Path,
    prefix: &Path,
    paths: &Paths<'_>,
    batch: &HashMap<(&str, &str), &Path>,
) -> Result<()> {
    let mut pending = vec![stage.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_unstable_by_key(|entry| entry.file_name());
        for entry in entries {
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                rewrite_file(stage, &path, prefix, paths, batch)
                    .with_context(|| format!("In staged file {}", path.display()))?;
            } else if !kind.is_symlink() {
                bail!("Unexpected entry in staged keg: {}", path.display());
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
) -> Result<()> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_RELOCATION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RELOCATION_FILE_BYTES {
        bail!("File exceeds the relocation size limit ({MAX_RELOCATION_FILE_BYTES} bytes)");
    }
    let relative = path.strip_prefix(stage)?;
    if macho_relocate::is_macho(&bytes) {
        macho_relocate::relocate(path, &bytes, |kind, old, rpaths| match kind {
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
                        bail!(
                            "Mach-O @rpath dependency {old:?} in {} does not resolve inside a pinned keg LC_RPATH",
                            relative.display()
                        );
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
    } else if let Some(rewritten) = relocation_text::rewrite(relative, &bytes, paths)? {
        fs::write(path, rewritten)?;
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
) -> Result<()> {
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
        .context("A relocated Mach-O reference must name a pinned keg target")?;
    if matches!(kind, TargetKind::File) && member.is_none_or(str::is_empty) {
        bail!("A relocated Mach-O reference must name a file within a pinned keg");
    }
    let installed_keg: PathBuf;
    let root = match batch.get(&(name, version)) {
        Some(stage) => *stage,
        None => {
            let cellar = prefix.join("Cellar");
            let package = cellar.join(name);
            installed_keg = package.join(version);
            for path in [&cellar, &package, &installed_keg] {
                if !fs::symlink_metadata(path)?.is_dir() {
                    bail!(
                        "Relocated library target is not inside a real managed keg: {}",
                        target.display()
                    );
                }
            }
            &installed_keg
        }
    };
    let root = root.canonicalize()?;
    let candidate = match member {
        Some(member) if !member.is_empty() => root.join(member),
        _ => root.clone(),
    };
    let real = candidate
        .canonicalize()
        .with_context(|| format!("Missing relocated library target {}", target.display()))?;
    let type_ok = match kind {
        TargetKind::File => real.is_file(),
        TargetKind::Dir => real.is_dir(),
    };
    if !real.starts_with(&root) || !type_ok {
        let kind_label = match kind {
            TargetKind::File => "regular file",
            TargetKind::Dir => "directory",
        };
        bail!(
            "Relocated library target is not a {kind_label} inside its pinned keg: {}",
            target.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::store::{Receipt, Store};

    #[test]
    fn rejected_relocation_never_publishes_the_keg_or_receipt() {
        let sandbox = tempfile::tempdir().unwrap();
        let prefix = sandbox.path().join("Application Support").join("native");
        let store = Store::open(&prefix, true).unwrap();
        let stage = store.stage().unwrap();
        let staged_path = stage.path().to_path_buf();
        fs::write(
            stage.path().join("opaque.dat"),
            b"\0@@HOMEBREW_PREFIX@@/lib",
        )
        .unwrap();
        let package = Prepared {
            stage,
            receipt: Receipt {
                name: "ripgrep".into(),
                version: "16.0.0".into(),
                version_scheme: 0,
                sha256: "a".repeat(64),
                requested: true,
                keg_only: false,
                conflicts_with: Vec::new(),
                dependencies: Vec::new(),
            },
        };
        let result = staged(
            &prefix,
            [Item {
                package: &package,
                needs_relocation: true,
            }],
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("opaque.dat"));
        assert!(store.installed().unwrap().is_empty());
        assert!(!prefix.join("Cellar/ripgrep").exists());
        drop(package);
        assert!(!staged_path.exists());
    }

    #[test]
    fn validates_staged_and_installed_targets_without_following_escapes() {
        let prefix = tempfile::tempdir().unwrap();
        let staged = tempfile::tempdir().unwrap();
        let dst = prefix.path().join("Cellar/pcre2/10.49/lib/libpcre2.dylib");
        let keg_root_dst = prefix.path().join("Cellar/pcre2/10.49");
        let lib_dir_dst = prefix.path().join("Cellar/pcre2/10.49/lib");
        let batch = HashMap::from([(("pcre2", "10.49"), staged.path())]);
        fs::create_dir(staged.path().join("lib")).unwrap();
        assert!(validate_target(prefix.path(), &dst, &batch, TargetKind::File).is_err());
        validate_target(prefix.path(), &lib_dir_dst, &batch, TargetKind::Dir).unwrap();
        validate_target(prefix.path(), &keg_root_dst, &batch, TargetKind::Dir).unwrap();
        #[cfg(unix)]
        {
            let outside = prefix.path().join("foreign.dylib");
            fs::write(&outside, b"unrelated").unwrap();
            std::os::unix::fs::symlink(&outside, staged.path().join("lib/escaped.dylib")).unwrap();
            assert!(
                validate_target(
                    prefix.path(),
                    &dst.with_file_name("escaped.dylib"),
                    &batch,
                    TargetKind::File
                )
                .is_err()
            );
            let outside_dir = prefix.path().join("foreign-dir");
            fs::create_dir(&outside_dir).unwrap();
            std::os::unix::fs::symlink(&outside_dir, staged.path().join("escaped-dir")).unwrap();
            assert!(
                validate_target(
                    prefix.path(),
                    &keg_root_dst.join("escaped-dir"),
                    &batch,
                    TargetKind::Dir
                )
                .is_err()
            );
        }
        fs::write(staged.path().join("lib/libpcre2.dylib"), b"fixture").unwrap();
        validate_target(prefix.path(), &dst, &batch, TargetKind::File).unwrap();
        assert!(validate_target(prefix.path(), &dst, &batch, TargetKind::Dir).is_err());
        let empty = HashMap::new();
        assert!(validate_target(prefix.path(), &dst, &empty, TargetKind::File).is_err());
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&dst, b"fixture").unwrap();
        validate_target(prefix.path(), &dst, &empty, TargetKind::File).unwrap();
        validate_target(prefix.path(), &lib_dir_dst, &empty, TargetKind::Dir).unwrap();
        assert!(
            validate_target(
                prefix.path(),
                dst.parent().unwrap(),
                &empty,
                TargetKind::File
            )
            .is_err()
        );
    }
}
