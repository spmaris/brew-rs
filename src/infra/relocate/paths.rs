//! Translate Homebrew load paths only when the transaction pins their kegs.

use std::path::Path;

use crate::infra::store::Receipt;

mod error;

pub(crate) use error::PathsError;

const CELLAR: &str = "@@HOMEBREW_CELLAR@@/";
const OPT: &str = "@@HOMEBREW_PREFIX@@/opt/";

/// Destination of a validated `LC_RPATH` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RpathTarget {
    /// A directory inside a pinned keg (`{prefix}/Cellar/<name>/<version>[/<dir>]`).
    KegDir(String),
    /// The managed prefix's `lib` directory (`{prefix}/lib`).
    PrefixLib(String),
}

/// Bind each placeholder to the package being staged, or to one of its exact
/// direct dependency receipts. Do not rely on `opt` links, the host's Homebrew
/// prefix, environment settings, or a mutable "current version" alias.
pub(super) struct Paths<'a> {
    prefix: &'a str,
    receipt: &'a Receipt,
}

impl<'a> Paths<'a> {
    pub(super) fn new(prefix: &'a Path, receipt: &'a Receipt) -> Result<Self, PathsError> {
        let prefix = prefix.to_str().ok_or(PathsError::PrefixNotUtf8)?;
        if !prefix.starts_with('/')
            || prefix[1..]
                .split('/')
                .any(|part| !safe_component(part, true))
        {
            return Err(PathsError::PrefixNotNormalized);
        }
        Ok(Self { prefix, receipt })
    }

    pub(super) fn prefix(&self) -> &str {
        self.prefix
    }

    /// Parse an `@rpath/<member>` dependency reference into its normalized keg-relative suffix.
    pub(super) fn rpath_member(path: &str) -> Result<Option<&str>, PathsError> {
        let Some(member) = path.strip_prefix("@rpath/") else {
            return Ok(None);
        };
        if member.is_empty() || !safe_relative(member) {
            return Err(PathsError::UnsafeRpathLibrary {
                path: path.to_owned(),
            });
        }
        Ok(Some(member))
    }

    /// Resolve an `LC_RPATH` entry for a Mach-O image at keg-relative `image_rel`.
    pub(super) fn rpath_path(
        &self,
        path: &str,
        image_rel: &Path,
    ) -> Result<RpathTarget, PathsError> {
        if path.starts_with(CELLAR) || path.starts_with(OPT) {
            let mapped = self
                .keg_path(path)?
                .ok_or(PathsError::UnsupportedCellarRpath)?;
            return Ok(RpathTarget::KegDir(mapped));
        }
        if path == "@loader_path" || path.starts_with("@loader_path/") {
            let stack = self.resolve_loader_path(path, image_rel)?;
            if stack.len() >= 3
                && stack[0] == "Cellar"
                && stack[1] == self.receipt.name
                && stack[2] == self.receipt.version
            {
                return Ok(RpathTarget::KegDir(format!(
                    "{}/{}",
                    self.prefix,
                    stack.join("/")
                )));
            }
            if stack.len() == 1 && stack[0] == "lib" {
                return Ok(RpathTarget::PrefixLib(format!("{}/lib", self.prefix)));
            }
            return Err(PathsError::UnsupportedLoaderRpath {
                path: path.to_owned(),
            });
        }
        Err(PathsError::UnsupportedRpath {
            path: path.to_owned(),
        })
    }

    /// A path read from a supported `LC_ID_DYLIB` or dependent-dylib load
    /// command. Refuse `@rpath/` (handled by caller against `LC_RPATH`), other
    /// dyld variables, and arbitrary absolute paths.
    pub(super) fn macho_path(
        &self,
        path: &str,
        image_rel: &Path,
    ) -> Result<Option<String>, PathsError> {
        if let Some(path) = path
            .strip_prefix("/usr/lib/")
            .or_else(|| path.strip_prefix("/System/Library/"))
        {
            if safe_relative(path) {
                return Ok(None);
            }
            return Err(PathsError::UnsafeSystemLibrary);
        }
        if path.starts_with("@loader_path/") {
            let stack = self.resolve_loader_path(path, image_rel)?;
            if stack.len() >= 4
                && stack[0] == "Cellar"
                && stack[1] == self.receipt.name
                && stack[2] == self.receipt.version
            {
                return Ok(Some(format!("{}/{}", self.prefix, stack.join("/"))));
            }
            return Err(PathsError::LoaderPathOutsideKeg {
                path: path.to_owned(),
            });
        }
        let destination = self.keg_path(path)?;
        if destination.is_none() {
            return Err(PathsError::UnsupportedMachoPath {
                path: path.to_owned(),
            });
        }
        Ok(destination)
    }

    /// Lexically resolve `@loader_path` or `@loader_path/<rel>` relative to
    /// `Cellar/<name>/<version>/<image_rel.parent()>`.
    fn resolve_loader_path(&self, raw: &str, image_rel: &Path) -> Result<Vec<String>, PathsError> {
        if !safe_component(&self.receipt.name, false)
            || !safe_component(&self.receipt.version, false)
        {
            return Err(PathsError::InvalidKegIdentity);
        }
        let mut stack = vec![
            "Cellar".to_owned(),
            self.receipt.name.clone(),
            self.receipt.version.clone(),
        ];
        if let Some(parent) = image_rel.parent() {
            for component in parent.components() {
                let std::path::Component::Normal(part) = component else {
                    return Err(PathsError::UnsupportedImagePath {
                        path: image_rel.to_path_buf(),
                    });
                };
                let part = part.to_str().ok_or(PathsError::ImagePathNotUtf8)?;
                if !safe_component(part, false) {
                    return Err(PathsError::UnsafeImageComponent {
                        part: part.to_owned(),
                    });
                }
                stack.push(part.to_owned());
            }
        }

        let rest = if raw == "@loader_path" {
            None
        } else {
            Some(
                raw.strip_prefix("@loader_path/")
                    .ok_or(PathsError::MissingLoaderPrefix)?,
            )
        };
        let mut seen_normal = false;
        if let Some(rest) = rest {
            for seg in rest.split('/') {
                if seg.is_empty() {
                    return Err(PathsError::EmptyLoaderSegment {
                        raw: raw.to_owned(),
                    });
                }
                match seg {
                    "." => {}
                    ".." => {
                        if seen_normal {
                            return Err(PathsError::LoaderParentAfterNormal {
                                raw: raw.to_owned(),
                            });
                        }
                        if stack.pop().is_none() {
                            return Err(PathsError::LoaderEscapesPrefix {
                                raw: raw.to_owned(),
                            });
                        }
                    }
                    other => {
                        if !safe_component(other, false) {
                            return Err(PathsError::UnsafeLoaderComponent {
                                component: other.to_owned(),
                                raw: raw.to_owned(),
                            });
                        }
                        seen_normal = true;
                        stack.push(other.to_owned());
                    }
                }
            }
        }
        if stack.is_empty() {
            return Err(PathsError::LoaderResolvesToRoot {
                raw: raw.to_owned(),
            });
        }
        Ok(stack)
    }

    /// Map complete kegs or members (e.g. a `.pc` variable or load path).
    pub(super) fn keg_path(&self, path: &str) -> Result<Option<String>, PathsError> {
        let (name, original_version, suffix) = if let Some(rest) = path.strip_prefix(CELLAR) {
            let (name, rest) = rest
                .split_once('/')
                .ok_or(PathsError::MissingCellarVersion)?;
            let (version, suffix) = split_member(rest)?;
            (name, Some(version), suffix)
        } else if let Some(rest) = path.strip_prefix(OPT) {
            let (name, suffix) = split_member(rest)?;
            (name, None, suffix)
        } else {
            return Ok(None);
        };
        let version = if name == self.receipt.name {
            self.receipt.version.as_str()
        } else {
            self.receipt
                .dependencies
                .iter()
                .find(|dependency| dependency.name == name)
                .map(|dependency| dependency.version.as_str())
                .ok_or_else(|| PathsError::UndeclaredKeg {
                    name: name.to_owned(),
                })?
        };
        if !safe_component(name, false) || !safe_component(version, false) {
            return Err(PathsError::InvalidKegIdentity);
        }
        if let Some(original) = original_version
            && original != version
        {
            return Err(PathsError::VersionMismatch {
                name: name.to_owned(),
                original: original.to_owned(),
                pinned: version.to_owned(),
            });
        }
        if let Some(suffix) = suffix {
            return Ok(Some(format!(
                "{}/Cellar/{name}/{version}/{suffix}",
                self.prefix
            )));
        }
        Ok(Some(format!("{}/Cellar/{name}/{version}", self.prefix)))
    }
}

fn split_member(path: &str) -> Result<(&str, Option<&str>), PathsError> {
    match path.split_once('/') {
        Some((head, suffix)) if safe_relative(suffix) => Ok((head, Some(suffix))),
        Some(_) => Err(PathsError::UnsafeKegMember),
        None => Ok((path, None)),
    }
}

fn safe_relative(path: &str) -> bool {
    path.split('/').all(|part| safe_component(part, false))
}

fn safe_component(part: &str, allow_space: bool) -> bool {
    !matches!(part, "" | "." | "..")
        && part.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_' | b'+' | b'@')
                || (allow_space && byte == b' ')
        })
}

#[cfg(test)]
mod tests;
