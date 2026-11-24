//! Translate Homebrew load paths only when the transaction pins their kegs.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::infra::store::Receipt;

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
    pub(super) fn new(prefix: &'a Path, receipt: &'a Receipt) -> Result<Self> {
        let prefix = prefix
            .to_str()
            .context("Bottle relocation requires a UTF-8 prefix")?;
        if !prefix.starts_with('/')
            || prefix[1..]
                .split('/')
                .any(|part| !safe_component(part, true))
        {
            bail!(
                "Bottle relocation requires a normalized absolute prefix with safe path characters"
            );
        }
        Ok(Self { prefix, receipt })
    }

    pub(super) fn prefix(&self) -> &str {
        self.prefix
    }

    /// Parse an `@rpath/<member>` dependency reference into its normalized keg-relative suffix.
    pub(super) fn rpath_member(path: &str) -> Result<Option<&str>> {
        let Some(member) = path.strip_prefix("@rpath/") else {
            return Ok(None);
        };
        if member.is_empty() || !safe_relative(member) {
            bail!("Unsafe @rpath library path {path:?}");
        }
        Ok(Some(member))
    }

    /// Resolve an `LC_RPATH` entry for a Mach-O image at keg-relative `image_rel`.
    pub(super) fn rpath_path(&self, path: &str, image_rel: &Path) -> Result<RpathTarget> {
        if path.starts_with(CELLAR) || path.starts_with(OPT) {
            let mapped = self
                .keg_path(path)?
                .context("Unsupported Cellar or opt LC_RPATH")?;
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
            bail!("Unsupported @loader_path LC_RPATH {path:?}");
        }
        bail!("Unsupported LC_RPATH {path:?}");
    }

    /// A path read from a supported `LC_ID_DYLIB` or dependent-dylib load
    /// command. Refuse `@rpath/` (handled by caller against `LC_RPATH`), other
    /// dyld variables, and arbitrary absolute paths.
    pub(super) fn macho_path(&self, path: &str, image_rel: &Path) -> Result<Option<String>> {
        if let Some(path) = path
            .strip_prefix("/usr/lib/")
            .or_else(|| path.strip_prefix("/System/Library/"))
        {
            if safe_relative(path) {
                return Ok(None);
            }
            bail!("Unsafe system library path in bottle");
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
            bail!("Mach-O @loader_path {path:?} must resolve to a member inside the same keg");
        }
        let destination = self.keg_path(path)?;
        if destination.is_none() {
            bail!(
                "Unsupported Mach-O path {path:?}: expected a pinned keg or macOS system library"
            );
        }
        Ok(destination)
    }

    /// Lexically resolve `@loader_path` or `@loader_path/<rel>` relative to
    /// `Cellar/<name>/<version>/<image_rel.parent()>`.
    fn resolve_loader_path(&self, raw: &str, image_rel: &Path) -> Result<Vec<String>> {
        if !safe_component(&self.receipt.name, false)
            || !safe_component(&self.receipt.version, false)
        {
            bail!("Invalid keg identity in relocation plan");
        }
        let mut stack = vec![
            "Cellar".to_owned(),
            self.receipt.name.clone(),
            self.receipt.version.clone(),
        ];
        if let Some(parent) = image_rel.parent() {
            for component in parent.components() {
                let std::path::Component::Normal(part) = component else {
                    bail!(
                        "Unsupported relative path for Mach-O image {}",
                        image_rel.display()
                    );
                };
                let part = part
                    .to_str()
                    .context("Mach-O image path is not valid UTF-8")?;
                if !safe_component(part, false) {
                    bail!("Unsafe Mach-O image path component {part:?}");
                }
                stack.push(part.to_owned());
            }
        }

        let rest = if raw == "@loader_path" {
            None
        } else {
            Some(
                raw.strip_prefix("@loader_path/")
                    .context("Expected @loader_path prefix")?,
            )
        };
        let mut seen_normal = false;
        if let Some(rest) = rest {
            for seg in rest.split('/') {
                if seg.is_empty() {
                    bail!("Empty path segment in @loader_path reference {raw:?}");
                }
                match seg {
                    "." => {}
                    ".." => {
                        if seen_normal {
                            bail!("Unsafe '..' after normal component in {raw:?}");
                        }
                        if stack.pop().is_none() {
                            bail!("Mach-O @loader_path {raw:?} escapes the prefix");
                        }
                    }
                    other => {
                        if !safe_component(other, false) {
                            bail!("Unsafe component {other:?} in @loader_path {raw:?}");
                        }
                        seen_normal = true;
                        stack.push(other.to_owned());
                    }
                }
            }
        }
        if stack.is_empty() {
            bail!("Mach-O @loader_path {raw:?} resolves to the prefix root");
        }
        Ok(stack)
    }

    /// Map complete kegs or members (e.g. a `.pc` variable or load path).
    pub(super) fn keg_path(&self, path: &str) -> Result<Option<String>> {
        let (name, original_version, suffix) = if let Some(rest) = path.strip_prefix(CELLAR) {
            let (name, rest) = rest
                .split_once('/')
                .context("Missing version in Cellar path")?;
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
                .with_context(|| format!("Bottle references undeclared keg {name:?}"))?
        };
        if !safe_component(name, false) || !safe_component(version, false) {
            bail!("Invalid keg identity in relocation plan");
        }
        if let Some(original) = original_version
            && original != version
        {
            bail!("Bottle requires {name} {original:?}, but the transaction pins {version:?}");
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

fn split_member(path: &str) -> Result<(&str, Option<&str>)> {
    match path.split_once('/') {
        Some((head, suffix)) if safe_relative(suffix) => Ok((head, Some(suffix))),
        Some(_) => bail!("Unsafe keg member path in bottle"),
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
