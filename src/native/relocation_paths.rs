//! Translate Homebrew load paths only when the transaction pins their kegs.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::store::Receipt;

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
mod tests {
    use super::*;
    use crate::native::store::Dependency;

    fn receipt() -> Receipt {
        Receipt {
            source: Default::default(),
            name: "ripgrep".into(),
            version: "16.0.0".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: vec![Dependency {
                name: "pcre2".into(),
                version: "10.49".into(),
                sha256: "b".repeat(64),
            }],
        }
    }

    #[test]
    fn relocates_opt_references_to_the_exact_pinned_keg() {
        let receipt = receipt();
        let paths = Paths::new(
            Path::new("/home/example/Application Support/native"),
            &receipt,
        )
        .unwrap();
        let img = Path::new("bin/ripgrep");
        assert_eq!(
            paths
                .macho_path("@@HOMEBREW_PREFIX@@/opt/pcre2/lib/libpcre2-8.0.dylib", img)
                .unwrap()
                .as_deref(),
            Some(
                "/home/example/Application Support/native/Cellar/pcre2/10.49/lib/libpcre2-8.0.dylib"
            )
        );
        assert_eq!(
            paths
                .keg_path("@@HOMEBREW_CELLAR@@/ripgrep/16.0.0")
                .unwrap()
                .as_deref(),
            Some("/home/example/Application Support/native/Cellar/ripgrep/16.0.0")
        );
        assert_eq!(
            paths
                .keg_path("@@HOMEBREW_CELLAR@@/pcre2/10.49/lib")
                .unwrap()
                .as_deref(),
            Some("/home/example/Application Support/native/Cellar/pcre2/10.49/lib")
        );
    }

    #[test]
    fn preserves_only_normalized_system_paths() {
        let receipt = receipt();
        let paths = Paths::new(Path::new("/tmp/prefix"), &receipt).unwrap();
        let img = Path::new("bin/ripgrep");
        for system in [
            "/usr/lib/libbz2.1.0.dylib",
            "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation",
        ] {
            assert_eq!(paths.macho_path(system, img).unwrap(), None);
        }
        for unsupported in [
            "/usr/lib/../../opt/homebrew/lib/evil.dylib",
            "/opt/homebrew/lib/libpcre2.dylib",
            "@rpath/library.dylib",
            "@@HOMEBREW_PREFIX@@/lib/library.dylib",
            "relative.dylib",
        ] {
            assert!(paths.macho_path(unsupported, img).is_err(), "{unsupported}");
        }
    }

    #[test]
    fn refuses_a_different_or_unpinned_keg_and_traversal() {
        let receipt = receipt();
        let paths = Paths::new(Path::new("/tmp/prefix"), &receipt).unwrap();
        let img = Path::new("bin/ripgrep");
        for malformed in [
            "@@HOMEBREW_CELLAR@@/pcre2/10.48/lib/libpcre2.dylib",
            "@@HOMEBREW_PREFIX@@/opt/openssl@3/lib/libssl.dylib",
            "@@HOMEBREW_PREFIX@@/opt/pcre2/../lib.dylib",
            "@@HOMEBREW_CELLAR@@/pcre2/10.49//lib",
            "@@HOMEBREW_CELLAR@@/pcre2/10.49/",
        ] {
            assert!(paths.macho_path(malformed, img).is_err(), "{malformed}");
        }
    }

    #[test]
    fn rejects_prefixes_unsafe_in_supported_text_metadata() {
        let receipt = receipt();
        for bad in [
            "relative",
            "/",
            "/tmp/../prefix",
            "/tmp//prefix",
            "/tmp/$HOME",
            "/tmp/'quoted'",
            "/tmp/line\nbreak",
        ] {
            assert!(Paths::new(Path::new(bad), &receipt).is_err(), "{bad}");
        }
    }

    #[test]
    fn resolves_rpaths_loader_paths_and_rpath_members() {
        let sqlite_receipt = Receipt {
            source: Default::default(),
            name: "sqlite".into(),
            version: "3.54.1".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: true,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        };
        let sqlite_paths = Paths::new(Path::new("/tmp/Space Prefix"), &sqlite_receipt).unwrap();
        assert_eq!(
            sqlite_paths
                .rpath_path(
                    "@@HOMEBREW_CELLAR@@/sqlite/3.54.1/lib",
                    Path::new("bin/sqlite3")
                )
                .unwrap(),
            RpathTarget::KegDir("/tmp/Space Prefix/Cellar/sqlite/3.54.1/lib".into())
        );

        let zstd_receipt = Receipt {
            source: Default::default(),
            name: "zstd".into(),
            version: "1.5.7_1".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        };
        let zstd_paths = Paths::new(Path::new("/tmp/Space Prefix"), &zstd_receipt).unwrap();
        assert_eq!(
            zstd_paths
                .rpath_path("@loader_path/../lib", Path::new("bin/zstd"))
                .unwrap(),
            RpathTarget::KegDir("/tmp/Space Prefix/Cellar/zstd/1.5.7_1/lib".into())
        );
        assert_eq!(
            zstd_paths
                .rpath_path("@loader_path/../lib", Path::new("lib/libzstd.1.5.7.dylib"))
                .unwrap(),
            RpathTarget::KegDir("/tmp/Space Prefix/Cellar/zstd/1.5.7_1/lib".into())
        );

        let luajit_receipt = Receipt {
            source: Default::default(),
            name: "luajit".into(),
            version: "2.1.1795119671".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: false,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        };
        let luajit_paths = Paths::new(Path::new("/tmp/Space Prefix"), &luajit_receipt).unwrap();
        assert_eq!(
            luajit_paths
                .rpath_path("@loader_path/../../../../lib", Path::new("bin/luajit-2.1"))
                .unwrap(),
            RpathTarget::PrefixLib("/tmp/Space Prefix/lib".into())
        );
        assert_eq!(
            luajit_paths
                .rpath_path(
                    "@loader_path/../../../../lib",
                    Path::new("lib/libluajit-5.1.dylib")
                )
                .unwrap(),
            RpathTarget::PrefixLib("/tmp/Space Prefix/lib".into())
        );
        assert!(
            luajit_paths
                .rpath_path(
                    "@loader_path/../../../../../etc",
                    Path::new("bin/luajit-2.1")
                )
                .is_err()
        );
        assert!(
            luajit_paths
                .rpath_path(
                    "@loader_path/../../../../share",
                    Path::new("bin/luajit-2.1")
                )
                .is_err()
        );

        let icu_receipt = Receipt {
            source: Default::default(),
            name: "icu4c@79".into(),
            version: "79.1".into(),
            version_scheme: 0,
            sha256: "a".repeat(64),
            requested: true,
            keg_only: true,
            conflicts_with: Vec::new(),
            dependencies: Vec::new(),
        };
        let icu_paths = Paths::new(Path::new("/tmp/Space Prefix"), &icu_receipt).unwrap();
        assert_eq!(
            icu_paths
                .macho_path(
                    "@loader_path/libicuuc.79.dylib",
                    Path::new("lib/libicui18n.79.1.dylib")
                )
                .unwrap()
                .as_deref(),
            Some("/tmp/Space Prefix/Cellar/icu4c@79/79.1/lib/libicuuc.79.dylib")
        );
        assert!(
            icu_paths
                .macho_path(
                    "@loader_path/../../other.dylib",
                    Path::new("lib/libicui18n.79.1.dylib")
                )
                .is_err()
        );

        assert_eq!(
            Paths::rpath_member("@rpath/libzstd.1.dylib").unwrap(),
            Some("libzstd.1.dylib")
        );
        assert_eq!(
            Paths::rpath_member("/usr/lib/libSystem.B.dylib").unwrap(),
            None
        );
        assert!(Paths::rpath_member("@rpath/").is_err());
        assert!(Paths::rpath_member("@rpath/../evil.dylib").is_err());
    }
}
