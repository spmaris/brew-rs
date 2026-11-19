//! Bounded extraction of a verified Homebrew bottle archive.
//!
//! A bottle is a gzipped tar whose real layout is
//! `<formula>/<version>/...`, with the formula's Ruby definition at
//! `<formula>/<version>/.brew/<formula>.rb`. [`unpack`] verifies the caller's
//! SHA-256 against a private snapshot of the exact bytes it decompresses,
//! and extracts regular files, directories, and accepted relative symlinks
//! into the caller-owned, empty staging directory. The keg
//! contents are written at the root of `out`; the `<formula>/<version>/`
//! prefix is required and stripped.
//!
//! `.brew` metadata is skipped rather than extracted. Relative symlinks are
//! accepted only when their target resolves lexically inside the keg and names
//! a regular file or directory that was already staged. Link creation is
//! deferred until every regular file and directory is written and the gzip
//! stream has been fully verified, so extraction never traverses an archive
//! link. Absolute, escaping, chained, cyclic, and broken links are rejected,
//! as are hardlinks, special files, duplicate paths, unsafe path components,
//! and archives that exceed the count, depth, path, or size budgets.
//! Ownership, set-id bits, timestamps, group/world write permission, and
//! symlink modes are never applied; executable bits are preserved.
//!
//! [`unpack`] only fills the keg. Keg-internal links such as
//! `lib/libfoo.dylib -> libfoo.1.dylib` are supported, but the prefix-level
//! links Homebrew installs in `bin`, `sbin`, or `opt` (for example
//! `<prefix>/bin/hello -> ../Cellar/hello/<version>/bin/hello`) leave the keg
//! and are rejected here. A caller that wants them must create them outside
//! this verifier, only for regular executables it inspected after extraction;
//! it must not copy an archive link out of the keg. This module exposes no
//! option that relaxes containment, and no staged file is ever executed.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::bufread::GzDecoder;
use sha2::{Digest, Sha256};
use tar::EntryType;

/// Length of a SHA-256 digest in lowercase hexadecimal.
const SHA256_HEX_LEN: usize = 64;
/// Longest formula name accepted in an archive path.
const MAX_NAME_LEN: usize = 128;
/// Longest version accepted in an archive path.
const MAX_VERSION_LEN: usize = 128;
/// Largest compressed archive that will be hashed or decompressed.
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest single extracted file.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Largest total expanded archive, including tar framing.
const MAX_DECOMPRESSED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Most entries accepted.
const MAX_ENTRIES: usize = 200_000;
/// Deepest path below `<name>/<version>/` accepted.
const MAX_DEPTH: usize = 64;
/// Longest individual path component accepted.
const MAX_COMPONENT_BYTES: usize = 255;
/// Longest symlink target string accepted, in bytes.
const MAX_LINK_TARGET_BYTES: usize = 1024;
/// Most components accepted in one symlink target, including `..`.
const MAX_LINK_COMPONENTS: usize = 2 * MAX_DEPTH;
/// Most symlinks accepted in one bottle.
const MAX_SYMLINKS: usize = 20_000;
/// Buffer used for hashing.
const IO_BUFFER_BYTES: usize = 64 * 1024;

/// Extract the keg contents of a verified bottle archive into `out`.
///
/// `out` must be an existing, empty directory owned by the caller. It is
/// filled with the contents of `<name>/<version>/`; `.brew` is skipped and
/// contained relative symlinks are created last. On error the caller should
/// discard `out`. `sha256` must be exactly 64 lowercase hexadecimal characters
/// and match the compressed archive bytes.
pub(super) fn unpack(
    archive: &Path,
    out: &Path,
    name: &str,
    version: &str,
    sha256: &str,
) -> Result<()> {
    validate_digest(sha256)?;
    validate_name(name)?;
    validate_version(version)?;

    let out_metadata = fs::symlink_metadata(out)
        .with_context(|| format!("failed to inspect staging directory {}", out.display()))?;
    if !out_metadata.file_type().is_dir() {
        bail!("staging path {} is not a regular directory", out.display());
    }
    let mut entries = fs::read_dir(out)
        .with_context(|| format!("failed to read staging directory {}", out.display()))?;
    if entries.next().is_some() {
        bail!(
            "staging directory {} is not empty; refusing to extract",
            out.display()
        );
    }

    let snapshot = verified_snapshot(archive, out, sha256)?;
    let decoder = GzDecoder::new(BufReader::new(snapshot));
    let mut tar = tar::Archive::new(LimitedReader::new(decoder, MAX_DECOMPRESSED_BYTES));
    let symlinks = extract(&mut tar, out, name, version)?;

    // The tar parser stops at the first end marker. Consume the zero padding
    // to enforce the total expansion limit and force gzip checksum validation.
    let mut tail = tar.into_inner();
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    loop {
        let read = tail
            .read(&mut buffer)
            .context("failed to finish the gzip stream")?;
        if read == 0 {
            break;
        }
        if buffer[..read].iter().any(|byte| *byte != 0) {
            bail!("unexpected data after the tar end marker");
        }
    }
    if !tail.inner.into_inner().fill_buf()?.is_empty() {
        bail!("unexpected bytes after the gzip stream");
    }

    // Only now, with every regular entry written and the archive verified, are
    // the validated links materialized. Nothing in `out` follows a link before
    // this point.
    materialize_symlinks(out, &symlinks)
}

/// `O_NOFOLLOW | O_NONBLOCK` where known, so a swapped-in symlink is not
/// followed and a swapped-in FIFO cannot block the open. The handle is still
/// required to be a regular file below.
#[cfg(unix)]
fn no_follow_flags() -> i32 {
    #[cfg(target_os = "macos")]
    {
        0x0100 | 0x0004
    }
    #[cfg(target_os = "linux")]
    {
        0o400000 | 0o4000
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

/// Copy the archive into private staging while checking its exact bytes.
fn verified_snapshot(archive: &Path, out: &Path, sha256: &str) -> Result<File> {
    let before = fs::symlink_metadata(archive)
        .with_context(|| format!("failed to inspect bottle archive {}", archive.display()))?;
    if !before.file_type().is_file() {
        bail!("bottle archive {} is not a regular file", archive.display());
    }
    if before.len() == 0 || before.len() > MAX_ARCHIVE_BYTES {
        bail!(
            "bottle archive {} is empty or exceeds the {MAX_ARCHIVE_BYTES} byte limit",
            archive.display()
        );
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(no_follow_flags());
    }
    let mut file = options
        .open(archive)
        .with_context(|| format!("failed to open bottle archive {}", archive.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("failed to inspect opened archive {}", archive.display()))?;
    if !opened.is_file() || opened.len() != before.len() {
        bail!(
            "bottle archive {} changed while it was being opened",
            archive.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != before.dev() || opened.ino() != before.ino() {
            bail!(
                "bottle archive {} was replaced while it was being opened",
                archive.display()
            );
        }
    }

    let mut snapshot =
        tempfile::tempfile_in(out).context("failed to create a private bottle snapshot")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to hash bottle archive {}", archive.display()))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .context("bottle archive size overflowed")?;
        if total > MAX_ARCHIVE_BYTES {
            bail!(
                "bottle archive {} exceeds the {MAX_ARCHIVE_BYTES} byte limit",
                archive.display()
            );
        }
        snapshot
            .write_all(&buffer[..read])
            .context("failed to copy the bottle snapshot")?;
        hasher.update(&buffer[..read]);
    }
    if total != opened.len() {
        bail!(
            "bottle archive {} changed while it was being read",
            archive.display()
        );
    }

    let actual = hex::encode(hasher.finalize());
    if actual != sha256 {
        bail!("SHA-256 mismatch for the bottle archive: expected {sha256}, got {actual}");
    }

    snapshot
        .seek(SeekFrom::Start(0))
        .context("failed to rewind the verified bottle snapshot")?;
    Ok(snapshot)
}

/// A syntactically and semantically validated symlink whose creation is
/// deferred until the archive is completely verified.
struct PendingSymlink {
    /// Keg-relative location of the link.
    path: PathBuf,
    /// Keg-relative path the link resolves to.
    target: PathBuf,
}

/// Walk the tar stream and materialize validated entries below `out`.
///
/// Valid symlinks are collected and returned rather than created, so the
/// caller never traverses them while the archive is still being read.
fn extract<R: Read>(
    tar: &mut tar::Archive<R>,
    out: &Path,
    name: &str,
    version: &str,
) -> Result<Vec<PendingSymlink>> {
    let mut seen: HashMap<PathBuf, Seen> = HashMap::new();
    let mut directories: HashSet<PathBuf> = HashSet::new();
    let mut symlinks: Vec<PendingSymlink> = Vec::new();
    let mut count = 0_usize;
    let mut declared_total = 0_u64;

    let entries = tar
        .entries()
        .context("failed to read the bottle archive as tar")?;
    for entry in entries {
        let mut entry = entry.context("failed to read a bottle archive entry")?;
        count = count
            .checked_add(1)
            .context("bottle archive entry count overflowed")?;
        if count > MAX_ENTRIES {
            bail!("bottle archive has more than {MAX_ENTRIES} entries");
        }

        let seen_kind = match entry.header().entry_type() {
            EntryType::Regular => Seen::File,
            EntryType::Directory => Seen::Dir,
            EntryType::Symlink => Seen::Symlink,
            other => bail!("unsupported bottle archive entry type {other:?}"),
        };
        let is_dir = seen_kind == Seen::Dir;

        let path_bytes = entry.path_bytes();
        let Some(relative) = relative_path(&path_bytes, name, version, is_dir)? else {
            continue;
        };
        if relative.components().count() > MAX_DEPTH {
            bail!(
                "bottle archive entry '{}' exceeds the depth limit of {MAX_DEPTH}",
                relative.display()
            );
        }

        // Vendor Ruby metadata lives in `<name>/<version>/.brew`. It is never
        // extracted or evaluated, so it must not count as a staged target.
        if relative.starts_with(".brew") {
            continue;
        }

        note_directories(&relative, &mut directories);
        if seen.insert(relative.clone(), seen_kind).is_some() {
            bail!(
                "bottle archive contains duplicate path '{}'",
                relative.display()
            );
        }

        check_ancestors(&relative, &seen)?;

        if seen_kind == Seen::Symlink {
            if directories.contains(&relative) {
                bail!(
                    "bottle archive symlink '{}' is used as an ancestor directory",
                    relative.display()
                );
            }
            if entry.size() != 0 {
                bail!(
                    "bottle archive symlink '{}' declares a nonzero size",
                    relative.display()
                );
            }
            let Some(target_bytes) = entry.link_name_bytes() else {
                bail!(
                    "bottle archive symlink '{}' has no target",
                    relative.display()
                );
            };
            let target = resolve_symlink_target(&relative, &target_bytes, &seen, &directories)?;
            if symlinks.len() >= MAX_SYMLINKS {
                bail!("bottle archive has more than {MAX_SYMLINKS} symlinks");
            }
            symlinks.push(PendingSymlink {
                path: relative,
                target,
            });
            continue;
        }

        let destination = out.join(&relative);

        if is_dir {
            create_directories(out, &relative)?;
            continue;
        }

        let size = entry.size();
        if size > MAX_FILE_BYTES {
            bail!(
                "bottle archive entry '{}' exceeds the {MAX_FILE_BYTES} byte limit",
                relative.display()
            );
        }
        declared_total = declared_total
            .checked_add(size)
            .context("bottle archive expanded size overflowed")?;
        if declared_total > MAX_DECOMPRESSED_BYTES {
            bail!("bottle archive expands beyond the {MAX_DECOMPRESSED_BYTES} byte limit");
        }

        if let Some(parent) = relative.parent() {
            create_directories(out, parent)?;
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&destination).with_context(|| {
            format!("failed to create extracted file {}", destination.display())
        })?;
        let copied = io::copy(&mut entry, &mut file)
            .with_context(|| format!("failed to extract file {}", destination.display()))?;
        if copied != size {
            bail!(
                "bottle archive entry '{}' was truncated: expected {size} bytes, got {copied}",
                relative.display()
            );
        }
        file.flush()
            .with_context(|| format!("failed to flush extracted file {}", destination.display()))?;
        drop(file);

        let mode = entry
            .header()
            .mode()
            .with_context(|| format!("failed to read the mode of '{}'", relative.display()))?;
        set_mode(&destination, 0o644 | (mode & 0o111))?;
    }

    Ok(symlinks)
}

/// Whether an archive path is an extracted file, directory, or deferred link.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    File,
    Dir,
    Symlink,
}

/// Record every proper ancestor directory of a validated entry path.
///
/// This tracks directories that were only created implicitly because a file or
/// link lives below them, so a later symlink cannot claim one of those paths.
fn note_directories(relative: &Path, directories: &mut HashSet<PathBuf>) {
    let mut current = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component);
        directories.insert(current.clone());
    }
}

/// Validate a symlink target and resolve it lexically to a keg-relative path.
///
/// Only relative targets that stay inside the keg are accepted. `..` may only
/// appear before any normal component, so a target can never rely on a
/// directory that a later `..` cancels. The resolved path must name a regular
/// file or directory that was already staged; chained links, links to links,
/// and broken links fail closed.
fn resolve_symlink_target(
    link: &Path,
    bytes: &[u8],
    seen: &HashMap<PathBuf, Seen>,
    directories: &HashSet<PathBuf>,
) -> Result<PathBuf> {
    if bytes.is_empty() {
        bail!(
            "bottle archive symlink '{}' has an empty target",
            link.display()
        );
    }
    if bytes.len() > MAX_LINK_TARGET_BYTES {
        bail!(
            "bottle archive symlink '{}' has a target longer than {MAX_LINK_TARGET_BYTES} bytes",
            link.display()
        );
    }
    if bytes
        .iter()
        .any(|byte| *byte == b'\\' || *byte == 0 || byte.is_ascii_control())
    {
        bail!(
            "bottle archive symlink '{}' has a target containing a backslash, NUL, or control byte",
            link.display()
        );
    }
    let target = std::str::from_utf8(bytes).ok().with_context(|| {
        format!(
            "bottle archive symlink '{}' has a target that is not valid UTF-8",
            link.display()
        )
    })?;
    if target.starts_with('/') {
        bail!(
            "bottle archive symlink '{}' has an absolute target",
            link.display()
        );
    }
    #[cfg(windows)]
    if target.starts_with('\\') || target.contains(':') {
        bail!(
            "bottle archive symlink '{}' has a Windows absolute or drive target",
            link.display()
        );
    }
    let components: Vec<&str> = target.split('/').collect();
    if components.len() > MAX_LINK_COMPONENTS {
        bail!(
            "bottle archive symlink '{}' has a target with too many components",
            link.display()
        );
    }
    if components.iter().any(|component| component.is_empty()) {
        bail!(
            "bottle archive symlink '{}' has a target with an empty component",
            link.display()
        );
    }
    if components
        .iter()
        .any(|component| component.len() > MAX_COMPONENT_BYTES)
    {
        bail!(
            "bottle archive symlink '{}' has a target with an overlong component",
            link.display()
        );
    }

    // The link name is a validated, normalized UTF-8 path, so its components
    // are safe to use as the lexical starting point.
    let link_name = link
        .to_str()
        .context("bottle archive symlink path is not valid UTF-8")?;
    let link_components: Vec<&str> = link_name
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let mut resolved: Vec<&str> = link_components
        .get(..link_components.len().saturating_sub(1))
        .unwrap_or_default()
        .to_vec();
    let mut normal_component = false;

    for component in components {
        match component {
            "." => {}
            ".." => {
                if normal_component {
                    bail!(
                        "bottle archive symlink '{}' has a target that uses '..' after a normal component",
                        link.display()
                    );
                }
                if resolved.pop().is_none() {
                    bail!(
                        "bottle archive symlink '{}' escapes the keg root",
                        link.display()
                    );
                }
            }
            other => {
                normal_component = true;
                if resolved.len() >= MAX_DEPTH {
                    bail!(
                        "bottle archive symlink '{}' has a target that exceeds the depth limit of {MAX_DEPTH}",
                        link.display()
                    );
                }
                resolved.push(other);
            }
        }
    }
    if resolved.is_empty() {
        bail!(
            "bottle archive symlink '{}' resolves to the keg root",
            link.display()
        );
    }

    let mut prefix = PathBuf::new();
    for (index, component) in resolved.iter().enumerate() {
        prefix.push(component);
        let is_leaf = index + 1 == resolved.len();
        match seen.get(&prefix) {
            Some(Seen::File) if is_leaf => {}
            Some(Seen::Dir) => {}
            Some(Seen::File) => bail!(
                "bottle archive symlink '{}' targets a path through a regular file",
                link.display()
            ),
            Some(Seen::Symlink) if is_leaf => bail!(
                "bottle archive symlink '{}' targets another symlink",
                link.display()
            ),
            Some(Seen::Symlink) => bail!(
                "bottle archive symlink '{}' targets a path through another symlink",
                link.display()
            ),
            None if directories.contains(&prefix) => {}
            None => bail!(
                "bottle archive symlink '{}' targets '{}', which was not staged before it",
                link.display(),
                prefix.display()
            ),
        }
    }
    // A directory link to one of its own ancestors can be followed forever
    // (for example `lib/sub/link -> ..`), so it is not a simple link.
    if link_components.starts_with(&resolved) {
        bail!(
            "bottle archive symlink '{}' targets an ancestor directory, which would form a cycle",
            link.display()
        );
    }
    Ok(prefix)
}

/// Create the deferred symlinks after all regular entries are in place.
fn materialize_symlinks(out: &Path, symlinks: &[PendingSymlink]) -> Result<()> {
    for link in symlinks {
        if let Some(parent) = link.path.parent() {
            if !parent.as_os_str().is_empty() {
                create_directories(out, parent)?;
            }
        }
        let destination = out.join(&link.path);
        if fs::symlink_metadata(&destination).is_ok() {
            bail!(
                "extraction path {} already exists; refusing to replace it with a symlink",
                destination.display()
            );
        }
        let target = relative_link_target(&link.path, &link.target);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&target, &destination).with_context(|| {
                format!(
                    "failed to create extracted symlink {}",
                    destination.display()
                )
            })?;
        }
        #[cfg(not(unix))]
        {
            let _ = (&target, &destination);
            bail!(
                "bottle archive symlink '{}' cannot be created on this platform",
                link.path.display()
            );
        }
    }
    Ok(())
}

/// Express a keg-relative target relative to the directory holding the link.
///
/// This keeps the on-disk link free of any component that a `..` elsewhere in
/// the target could cancel, so kernel path resolution matches the lexical
/// validation exactly.
fn relative_link_target(link: &Path, target: &Path) -> PathBuf {
    let parent: Vec<_> = link
        .parent()
        .map(|parent| parent.components().collect())
        .unwrap_or_default();
    let target: Vec<_> = target.components().collect();
    let common = parent
        .iter()
        .zip(target.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..parent.len() {
        relative.push("..");
    }
    for component in &target[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

/// Validate one resolved tar path and return its path below the keg root.
///
/// `None` is returned for the redundant `<name>` and `<name>/<version>`
/// directory entries, which are allowed as ancestors but not materialized.
fn relative_path(bytes: &[u8], name: &str, version: &str, is_dir: bool) -> Result<Option<PathBuf>> {
    let trailing_slash = bytes.ends_with(b"/");
    let bytes = if trailing_slash {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    if bytes.is_empty() {
        bail!("bottle archive contains an empty path");
    }
    if bytes
        .iter()
        .any(|byte| *byte == b'\\' || *byte == 0 || byte.is_ascii_control())
    {
        bail!(
            "bottle archive path '{}' contains a backslash, NUL, or control byte",
            String::from_utf8_lossy(bytes)
        );
    }
    let path = std::str::from_utf8(bytes)
        .ok()
        .context("bottle archive path is not valid UTF-8")?;
    let components: Vec<&str> = path.split('/').collect();
    if components
        .iter()
        .any(|component| component.is_empty() || *component == "." || *component == "..")
    {
        bail!("bottle archive path '{path}' is not a normalized relative path");
    }
    if components
        .iter()
        .any(|component| component.len() > MAX_COMPONENT_BYTES)
    {
        bail!("bottle archive path '{path}' contains an overlong component");
    }
    #[cfg(windows)]
    if components.iter().any(|component| component.contains(':')) {
        bail!("bottle archive path '{path}' contains a Windows drive prefix");
    }

    if components[0] != name {
        bail!("bottle archive path '{path}' is outside '{name}/'");
    }
    match components.len() {
        1 => {
            if !is_dir {
                bail!("bottle archive path '{path}' is outside the version directory");
            }
            Ok(None)
        }
        2 => {
            if components[1] != version {
                bail!("bottle archive path '{path}' is outside '{name}/{version}/'");
            }
            if !is_dir {
                bail!("bottle archive path '{path}' is not a directory");
            }
            Ok(None)
        }
        _ => {
            if components[1] != version {
                bail!("bottle archive path '{path}' is outside '{name}/{version}/'");
            }
            if trailing_slash && !is_dir {
                bail!("bottle archive file path '{path}' ends with a slash");
            }
            let mut relative = PathBuf::new();
            for component in &components[2..] {
                relative.push(component);
            }
            Ok(Some(relative))
        }
    }
}

/// Reject an entry nested under a path that was already extracted as a file,
/// or under a symlink that will only be created after extraction.
fn check_ancestors(relative: &Path, seen: &HashMap<PathBuf, Seen>) -> Result<()> {
    let mut ancestor = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        ancestor.push(component);
        match seen.get(&ancestor) {
            Some(Seen::File) => bail!(
                "bottle archive path '{}' is nested under an extracted file",
                relative.display()
            ),
            Some(Seen::Symlink) => bail!(
                "bottle archive path '{}' is nested under an extracted symlink",
                relative.display()
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Create each component of `relative` below `out` with a normalized mode.
fn create_directories(out: &Path, relative: &Path) -> Result<()> {
    let mut current = out.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => set_mode(&current, 0o755)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!("failed to inspect extraction path {}", current.display())
                })?;
                if !metadata.file_type().is_dir() {
                    bail!(
                        "extraction path {} is not a regular directory",
                        current.display()
                    );
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create directory {}", current.display()));
            }
        }
    }
    Ok(())
}

/// Apply a normalized permission mode where the platform supports one.
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Require exactly 64 lowercase hexadecimal characters.
fn validate_digest(sha256: &str) -> Result<()> {
    if sha256.len() != SHA256_HEX_LEN
        || !sha256
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        bail!("bottle sha256 must be exactly {SHA256_HEX_LEN} lowercase hexadecimal characters");
    }
    Ok(())
}

/// Require a safe Homebrew formula name.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        bail!("bottle name must be between 1 and {MAX_NAME_LEN} bytes");
    }
    if !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit() {
        bail!("bottle name '{name}' must start with a lowercase letter or digit");
    }
    if name.contains("..")
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'@' | b'+' | b'-' | b'_' | b'.')
        })
    {
        bail!("bottle name '{name}' contains unsupported characters");
    }
    Ok(())
}

/// Require a safe bottle version.
fn validate_version(version: &str) -> Result<()> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        bail!("bottle version must be between 1 and {MAX_VERSION_LEN} bytes");
    }
    if version.contains("..")
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
    {
        bail!("bottle version '{version}' contains unsupported characters");
    }
    Ok(())
}

/// A reader that fails if its input continues after a byte budget.
struct LimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R> LimitedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other(
                    "bottle archive expanded beyond the decompressed byte limit",
                )),
            };
        }
        let allowed = self.remaining.min(buffer.len() as u64) as usize;
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::tempdir;

    const NAME: &str = "hello";
    const VERSION: &str = "2.12.1";

    struct Fixture<'a> {
        path: &'a str,
        kind: FixtureKind,
        data: &'a [u8],
        mode: u32,
    }

    enum FixtureKind {
        File,
        Dir,
        Symlink(&'static str),
        Hardlink(&'static str),
        Fifo,
    }

    fn gzipped(entries: &[Fixture<'_>]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(entry.mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            match entry.kind {
                FixtureKind::File => {
                    header.set_entry_type(EntryType::Regular);
                    header.set_size(entry.data.len() as u64);
                    builder
                        .append_data(&mut header, entry.path, entry.data)
                        .expect("append file");
                }
                FixtureKind::Dir => {
                    header.set_entry_type(EntryType::Directory);
                    header.set_size(0);
                    builder
                        .append_data(&mut header, entry.path, io::empty())
                        .expect("append directory");
                }
                FixtureKind::Symlink(target) => {
                    header.set_entry_type(EntryType::Symlink);
                    header.set_size(0);
                    builder
                        .append_link(&mut header, entry.path, target)
                        .expect("append symlink");
                }
                FixtureKind::Hardlink(target) => {
                    header.set_entry_type(EntryType::Link);
                    header.set_size(0);
                    builder
                        .append_link(&mut header, entry.path, target)
                        .expect("append hardlink");
                }
                FixtureKind::Fifo => {
                    header.set_entry_type(EntryType::Fifo);
                    header.set_size(0);
                    builder
                        .append_data(&mut header, entry.path, io::empty())
                        .expect("append fifo");
                }
            }
        }
        builder.finish().expect("finish tar");
        builder
            .into_inner()
            .expect("finish gzip")
            .finish()
            .expect("gzip bytes")
    }

    /// Build a tar with an arbitrary (possibly hostile) header name.
    struct RawEntry {
        path: &'static [u8],
        kind: EntryType,
        size: u64,
        mode: u32,
        data: &'static [u8],
    }

    fn raw_gzipped(entries: &[RawEntry]) -> Vec<u8> {
        let mut tar = Vec::new();
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(entry.kind);
            header.set_mode(entry.mode);
            header.set_size(entry.size);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            assert!(entry.path.len() <= 100, "test header name must fit");
            header.as_old_mut().name[..entry.path.len()].copy_from_slice(entry.path);
            header.set_cksum();
            tar.extend_from_slice(header.as_bytes());
            tar.extend_from_slice(entry.data);
            let padding = (512 - entry.data.len() % 512) % 512;
            tar.extend(std::iter::repeat_n(0_u8, padding));
        }
        tar.extend(std::iter::repeat_n(0_u8, 1024));
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).expect("gzip tar");
        encoder.finish().expect("gzip bytes")
    }

    /// Build a tar entry with an arbitrary symlink header, including a link
    /// target that is not valid UTF-8 or a declared payload size.
    fn raw_symlink_gzipped(path: &[u8], target: &[u8], size: u64, data: &[u8]) -> Vec<u8> {
        let mut tar = Vec::new();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(size);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        assert!(
            path.len() <= 100 && target.len() <= 100,
            "test header fields must fit"
        );
        header.as_old_mut().name[..path.len()].copy_from_slice(path);
        header.as_old_mut().linkname[..target.len()].copy_from_slice(target);
        header.set_cksum();
        tar.extend_from_slice(header.as_bytes());
        tar.extend_from_slice(data);
        let padding = (512 - data.len() % 512) % 512;
        tar.extend(std::iter::repeat_n(0_u8, padding));
        tar.extend(std::iter::repeat_n(0_u8, 1024));
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).expect("gzip tar");
        encoder.finish().expect("gzip bytes")
    }

    #[cfg(unix)]
    fn is_symlink(path: &Path) -> bool {
        fs::symlink_metadata(path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    }

    struct Staged {
        archive: PathBuf,
        out: PathBuf,
        digest: String,
        _temp: tempfile::TempDir,
    }

    fn stage(bytes: &[u8]) -> Staged {
        let temp = tempdir().expect("temporary directory");
        let archive = temp.path().join("bottle.tar.gz");
        fs::write(&archive, bytes).expect("write archive");
        let out = temp.path().join("out");
        fs::create_dir(&out).expect("create staging directory");
        let digest = hex::encode(Sha256::digest(bytes));
        Staged {
            archive,
            out,
            digest,
            _temp: temp,
        }
    }

    fn real_shape() -> Vec<Fixture<'static>> {
        vec![
            Fixture {
                path: "hello/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/bin/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o777,
            },
            Fixture {
                path: "hello/2.12.1/bin/hello",
                kind: FixtureKind::File,
                data: b"#!/bin/sh\necho hello\n",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/share/doc/README",
                kind: FixtureKind::File,
                data: b"hello\n",
                mode: 0o644,
            },
            Fixture {
                path: "hello/2.12.1/.brew/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/.brew/hello.rb",
                kind: FixtureKind::File,
                data: b"class Hello < Formula\n",
                mode: 0o644,
            },
        ]
    }

    fn unpack_fixture(staged: &Staged) -> Result<()> {
        unpack(&staged.archive, &staged.out, NAME, VERSION, &staged.digest)
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).expect("metadata").permissions().mode() & 0o7777
    }

    #[test]
    fn extracts_the_real_bottle_shape_and_skips_brew_metadata() {
        let staged = stage(&gzipped(&real_shape()));
        unpack_fixture(&staged).expect("valid bottle");

        assert_eq!(
            fs::read(staged.out.join("bin/hello")).expect("read binary"),
            b"#!/bin/sh\necho hello\n"
        );
        assert_eq!(
            fs::read(staged.out.join("share/doc/README")).expect("read readme"),
            b"hello\n"
        );
        assert!(!staged.out.join(".brew").exists());
        assert!(!staged.out.join("hello").exists());
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&staged.out.join("bin/hello")), 0o755);
            assert_eq!(mode_of(&staged.out.join("share/doc/README")), 0o644);
            assert_eq!(mode_of(&staged.out.join("bin")), 0o755);
        }
    }

    #[test]
    fn accepts_a_bare_name_directory_entry() {
        let mut entries = vec![Fixture {
            path: "hello",
            kind: FixtureKind::Dir,
            data: b"",
            mode: 0o755,
        }];
        entries.extend(real_shape());
        let staged = stage(&gzipped(&entries));
        unpack_fixture(&staged).expect("optional leading name directory");
        assert!(staged.out.join("bin/hello").is_file());
    }

    #[test]
    fn supports_gnu_long_paths() {
        let directory = "d".repeat(110);
        let path = format!("hello/2.12.1/share/{directory}/file.txt");
        let entries = [Fixture {
            path: &path,
            kind: FixtureKind::File,
            data: b"long\n",
            mode: 0o644,
        }];
        let staged = stage(&gzipped(&entries));
        unpack_fixture(&staged).expect("long GNU path");
        assert_eq!(
            fs::read(staged.out.join("share").join(&directory).join("file.txt"))
                .expect("read long path"),
            b"long\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn normalizes_modes_without_setid_or_world_writes() {
        let entries = vec![
            Fixture {
                path: "hello/2.12.1/suid",
                kind: FixtureKind::File,
                data: b"x",
                mode: 0o6755,
            },
            Fixture {
                path: "hello/2.12.1/world",
                kind: FixtureKind::File,
                data: b"x",
                mode: 0o666,
            },
            Fixture {
                path: "hello/2.12.1/private",
                kind: FixtureKind::File,
                data: b"x",
                mode: 0o400,
            },
            Fixture {
                path: "hello/2.12.1/bin/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o777,
            },
            Fixture {
                path: "hello/2.12.1/deep/nested/file",
                kind: FixtureKind::File,
                data: b"x",
                mode: 0o644,
            },
        ];
        let staged = stage(&gzipped(&entries));
        unpack_fixture(&staged).expect("valid modes");

        assert_eq!(mode_of(&staged.out.join("suid")), 0o755);
        assert_eq!(mode_of(&staged.out.join("world")), 0o644);
        assert_eq!(mode_of(&staged.out.join("private")), 0o644);
        assert_eq!(mode_of(&staged.out.join("bin")), 0o755);
        assert_eq!(mode_of(&staged.out.join("deep")), 0o755);
        assert_eq!(mode_of(&staged.out.join("deep/nested")), 0o755);
    }

    #[test]
    fn rejects_a_checksum_mismatch_and_writes_nothing() {
        let staged = stage(&gzipped(&real_shape()));
        let error = unpack(&staged.archive, &staged.out, NAME, VERSION, &"b".repeat(64))
            .expect_err("wrong digest must fail");
        assert!(
            format!("{error:#}").contains("SHA-256 mismatch"),
            "{error:#}"
        );
        assert_eq!(fs::read_dir(&staged.out).expect("read dir").count(), 0);
    }

    #[test]
    fn rejects_trailing_bytes_even_when_the_digest_matches() {
        let mut bytes = gzipped(&real_shape());
        bytes.extend_from_slice(b"extra");
        let staged = stage(&bytes);
        let error = unpack_fixture(&staged).expect_err("trailing bytes must fail");
        assert!(format!("{error:#}").contains("unexpected bytes after the gzip stream"));
    }

    #[test]
    fn rejects_malformed_digests_before_opening() {
        let staged = stage(&gzipped(&real_shape()));
        for digest in [
            staged.digest.to_uppercase(),
            staged.digest[..63].to_string(),
            format!("{}g", &staged.digest[..63]),
            String::new(),
        ] {
            assert!(
                unpack(&staged.archive, &staged.out, NAME, VERSION, &digest).is_err(),
                "digest '{digest}' must be rejected"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlinked_archive() {
        use std::os::unix::fs::symlink;

        let staged = stage(&gzipped(&real_shape()));
        let link = staged.archive.with_file_name("link.tar.gz");
        symlink(&staged.archive, &link).expect("symlink archive");
        let error = unpack(&link, &staged.out, NAME, VERSION, &staged.digest)
            .expect_err("symlinked archive must fail");
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
    }

    #[test]
    fn rejects_a_non_empty_staging_directory() {
        let staged = stage(&gzipped(&real_shape()));
        fs::write(staged.out.join("existing"), b"x").expect("write existing file");
        let error = unpack_fixture(&staged).expect_err("non-empty staging must fail");
        assert!(format!("{error:#}").contains("not empty"), "{error:#}");
    }

    #[test]
    fn rejects_path_traversal_and_absolute_paths() {
        for path in [
            b"hello/2.12.1/../../escape".as_slice(),
            b"/hello/2.12.1/escape".as_slice(),
            b"hello/2.12.1/./escape".as_slice(),
            b"hello/2.12.1/..\\escape".as_slice(),
        ] {
            let bytes = raw_gzipped(&[RawEntry {
                path,
                kind: EntryType::Regular,
                size: 1,
                mode: 0o644,
                data: b"x",
            }]);
            let staged = stage(&bytes);
            let error = unpack_fixture(&staged).expect_err("malicious path must fail");
            let message = format!("{error:#}");
            assert!(
                message.contains("path")
                    || message.contains("outside")
                    || message.contains("normalized"),
                "{message}"
            );
            assert!(!staged.out.join("escape").exists());
            assert!(!staged.out.join("../escape").exists());
        }
    }

    #[test]
    fn rejects_paths_outside_the_name_and_version() {
        for path in [
            b"other/2.12.1/escape".as_slice(),
            b"hello/9.9.9/escape".as_slice(),
            b"hello/2.12.1".as_slice(),
        ] {
            let bytes = raw_gzipped(&[RawEntry {
                path,
                kind: EntryType::Regular,
                size: 1,
                mode: 0o644,
                data: b"x",
            }]);
            let staged = stage(&bytes);
            assert!(
                unpack_fixture(&staged).is_err(),
                "path '{}' must be rejected",
                String::from_utf8_lossy(path)
            );
        }
    }

    #[test]
    fn rejects_hardlinks_and_special_files() {
        let cases = [
            (
                "hardlink",
                vec![Fixture {
                    path: "hello/2.12.1/bin/evil",
                    kind: FixtureKind::Hardlink("hello/2.12.1/bin/hello"),
                    data: b"",
                    mode: 0o644,
                }],
            ),
            (
                "fifo",
                vec![Fixture {
                    path: "hello/2.12.1/evil",
                    kind: FixtureKind::Fifo,
                    data: b"",
                    mode: 0o644,
                }],
            ),
        ];

        for (label, entries) in cases {
            let staged = stage(&gzipped(&entries));
            let error = unpack_fixture(&staged).expect_err(label);
            assert!(
                format!("{error:#}").contains("unsupported bottle archive entry type"),
                "{label}: {error:#}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn extracts_contained_relative_symlinks() {
        let entries = vec![
            Fixture {
                path: "hello/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/lib/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/lib/libfoo.1.dylib",
                kind: FixtureKind::File,
                data: b"payload",
                mode: 0o644,
            },
            Fixture {
                path: "hello/2.12.1/lib/libfoo.dylib",
                kind: FixtureKind::Symlink("libfoo.1.dylib"),
                data: b"",
                mode: 0o777,
            },
            Fixture {
                path: "hello/2.12.1/share/real/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/share/real/data",
                kind: FixtureKind::File,
                data: b"data",
                mode: 0o644,
            },
            Fixture {
                path: "hello/2.12.1/share/latest",
                kind: FixtureKind::Symlink("real"),
                data: b"",
                mode: 0o777,
            },
            // `bin` has no directory entry of its own; the link parent is
            // created only when the deferred link is materialized.
            Fixture {
                path: "hello/2.12.1/bin/tool",
                kind: FixtureKind::Symlink("../lib/libfoo.1.dylib"),
                data: b"",
                mode: 0o777,
            },
        ];
        let staged = stage(&gzipped(&entries));
        unpack_fixture(&staged).expect("contained relative links");

        let lib_link = staged.out.join("lib/libfoo.dylib");
        assert!(is_symlink(&lib_link));
        assert_eq!(
            fs::read_link(&lib_link).expect("read lib link"),
            Path::new("libfoo.1.dylib")
        );
        assert_eq!(
            fs::read(&lib_link).expect("read through lib link"),
            b"payload"
        );

        let dir_link = staged.out.join("share/latest");
        assert!(is_symlink(&dir_link));
        assert_eq!(
            fs::read_link(&dir_link).expect("read dir link"),
            Path::new("real")
        );
        assert_eq!(
            fs::read(dir_link.join("data")).expect("read through dir link"),
            b"data"
        );

        let nested = staged.out.join("bin/tool");
        assert!(is_symlink(&nested));
        assert_eq!(
            fs::read_link(&nested).expect("read nested link"),
            Path::new("../lib/libfoo.1.dylib")
        );
        assert_eq!(
            fs::read(&nested).expect("read through nested link"),
            b"payload"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unsafe_symlink_targets() {
        let staged_link = |target: &'static str| {
            let entries = vec![
                Fixture {
                    path: "hello/2.12.1/lib/real",
                    kind: FixtureKind::File,
                    data: b"x",
                    mode: 0o644,
                },
                Fixture {
                    path: "hello/2.12.1/lib/link",
                    kind: FixtureKind::Symlink(target),
                    data: b"",
                    mode: 0o777,
                },
            ];
            stage(&gzipped(&entries))
        };

        for (target, expected) in [
            ("/tmp/escape", "absolute target"),
            ("../../escape", "escapes the keg root"),
            ("..", "resolves to the keg root"),
            ("..\\lib\\real", "backslash"),
            ("real/../lib", "'..' after a normal component"),
            ("missing", "was not staged before it"),
        ] {
            let staged = staged_link(target);
            let error = unpack_fixture(&staged).expect_err(target);
            assert!(
                format!("{error:#}").contains(expected),
                "target '{target}': {error:#}"
            );
            assert!(
                !is_symlink(&staged.out.join("lib/link")),
                "target '{target}' must not create a link"
            );
        }

        // `tar::Builder` normalizes `//` and trailing slashes, so build these
        // hostile target strings as raw headers.
        for target in [b"lib//real".as_slice(), b"lib/".as_slice()] {
            let bytes = raw_symlink_gzipped(b"hello/2.12.1/lib/link", target, 0, b"");
            let staged = stage(&bytes);
            let error = unpack_fixture(&staged).expect_err("empty target component");
            assert!(
                format!("{error:#}").contains("empty component"),
                "target '{}': {error:#}",
                String::from_utf8_lossy(target)
            );
        }

        // A link to its own directory or to another ancestor would form a
        // directory cycle.
        for (path, target) in [
            (b"hello/2.12.1/lib/sub/cycle".as_slice(), b"..".as_slice()),
            (b"hello/2.12.1/lib/selfdir".as_slice(), b".".as_slice()),
        ] {
            let bytes = raw_symlink_gzipped(path, target, 0, b"");
            let staged = stage(&bytes);
            let error = unpack_fixture(&staged).expect_err("ancestor target");
            assert!(
                format!("{error:#}").contains("ancestor directory"),
                "target '{}': {error:#}",
                String::from_utf8_lossy(target)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_empty_and_oversized_symlink_targets() {
        let non_utf8 = raw_symlink_gzipped(b"hello/2.12.1/lib/bad", b"real\xff", 0, b"");
        let staged = stage(&non_utf8);
        let error = unpack_fixture(&staged).expect_err("non-UTF-8 target");
        assert!(
            format!("{error:#}").contains("not valid UTF-8"),
            "{error:#}"
        );

        let no_target = raw_symlink_gzipped(b"hello/2.12.1/lib/none", b"", 0, b"");
        let staged = stage(&no_target);
        let error = unpack_fixture(&staged).expect_err("missing link name");
        assert!(format!("{error:#}").contains("has no target"), "{error:#}");

        let overlong = "a".repeat(MAX_LINK_TARGET_BYTES + 1);
        let many = vec!["d"; MAX_LINK_COMPONENTS + 1].join("/");
        for (target, expected) in [
            (
                Box::leak(overlong.into_boxed_str()) as &'static str,
                "longer than",
            ),
            (
                Box::leak(many.into_boxed_str()) as &'static str,
                "too many components",
            ),
        ] {
            let entries = [Fixture {
                path: "hello/2.12.1/lib/link",
                kind: FixtureKind::Symlink(target),
                data: b"",
                mode: 0o777,
            }];
            let staged = stage(&gzipped(&entries));
            let error = unpack_fixture(&staged).expect_err("bounded target");
            assert!(
                format!("{error:#}").contains(expected),
                "target length {}: {error:#}",
                target.len()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_chained_cyclic_and_duplicate_symlinks() {
        let cases = [
            (
                "chained",
                vec![
                    Fixture {
                        path: "hello/2.12.1/lib/real",
                        kind: FixtureKind::File,
                        data: b"x",
                        mode: 0o644,
                    },
                    Fixture {
                        path: "hello/2.12.1/lib/first",
                        kind: FixtureKind::Symlink("real"),
                        data: b"",
                        mode: 0o777,
                    },
                    Fixture {
                        path: "hello/2.12.1/lib/second",
                        kind: FixtureKind::Symlink("first"),
                        data: b"",
                        mode: 0o777,
                    },
                ],
                "another symlink",
            ),
            (
                "cyclic",
                vec![Fixture {
                    path: "hello/2.12.1/lib/self",
                    kind: FixtureKind::Symlink("self"),
                    data: b"",
                    mode: 0o777,
                }],
                "another symlink",
            ),
            (
                "duplicate",
                vec![
                    Fixture {
                        path: "hello/2.12.1/lib/real",
                        kind: FixtureKind::File,
                        data: b"x",
                        mode: 0o644,
                    },
                    Fixture {
                        path: "hello/2.12.1/lib/link",
                        kind: FixtureKind::Symlink("real"),
                        data: b"",
                        mode: 0o777,
                    },
                    Fixture {
                        path: "hello/2.12.1/lib/link",
                        kind: FixtureKind::Symlink("real"),
                        data: b"",
                        mode: 0o777,
                    },
                ],
                "duplicate",
            ),
        ];

        for (label, entries, expected) in cases {
            let staged = stage(&gzipped(&entries));
            let error = unpack_fixture(&staged).expect_err(label);
            assert!(
                format!("{error:#}").contains(expected),
                "{label}: {error:#}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_used_as_ancestors() {
        // A link declared before an entry that would be written through it.
        let link_first = vec![
            Fixture {
                path: "hello/2.12.1/lib/real/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/lib/real/data",
                kind: FixtureKind::File,
                data: b"x",
                mode: 0o644,
            },
            Fixture {
                path: "hello/2.12.1/lib/alias",
                kind: FixtureKind::Symlink("real"),
                data: b"",
                mode: 0o777,
            },
            Fixture {
                path: "hello/2.12.1/lib/alias/evil",
                kind: FixtureKind::File,
                data: b"evil",
                mode: 0o644,
            },
        ];
        let staged = stage(&gzipped(&link_first));
        let error = unpack_fixture(&staged).expect_err("entry below a link");
        assert!(
            format!("{error:#}").contains("nested under an extracted symlink"),
            "{error:#}"
        );
        assert!(!staged.out.join("lib/real/evil").exists());
        assert!(!is_symlink(&staged.out.join("lib/alias")));

        // A link whose path is already an implicitly created directory.
        let link_after = vec![
            Fixture {
                path: "hello/2.12.1/lib/real/",
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            },
            Fixture {
                path: "hello/2.12.1/lib/dir/file",
                kind: FixtureKind::File,
                data: b"x",
                mode: 0o644,
            },
            Fixture {
                path: "hello/2.12.1/lib/dir",
                kind: FixtureKind::Symlink("real"),
                data: b"",
                mode: 0o777,
            },
        ];
        let staged = stage(&gzipped(&link_after));
        let error = unpack_fixture(&staged).expect_err("link over a directory");
        assert!(
            format!("{error:#}").contains("used as an ancestor directory"),
            "{error:#}"
        );
        assert!(!is_symlink(&staged.out.join("lib/dir")));
    }

    #[cfg(unix)]
    #[test]
    fn defers_symlink_creation_until_verification_completes() {
        let mut bytes = gzipped(&[
            Fixture {
                path: "hello/2.12.1/lib/real",
                kind: FixtureKind::File,
                data: b"x",
                mode: 0o644,
            },
            Fixture {
                path: "hello/2.12.1/lib/link",
                kind: FixtureKind::Symlink("real"),
                data: b"",
                mode: 0o777,
            },
        ]);
        bytes.extend_from_slice(b"extra");
        let staged = stage(&bytes);
        let error = unpack_fixture(&staged).expect_err("trailing bytes must fail");
        assert!(
            format!("{error:#}").contains("unexpected bytes after the gzip stream"),
            "{error:#}"
        );
        assert!(!is_symlink(&staged.out.join("lib/link")));
        assert!(!staged.out.join("lib/link").exists());
    }

    #[test]
    fn rejects_symlinks_with_a_payload() {
        let bytes = raw_symlink_gzipped(b"hello/2.12.1/lib/evil", b"real", 1, b"x");
        let staged = stage(&bytes);
        let error = unpack_fixture(&staged).expect_err("symlink payload must fail");
        assert!(format!("{error:#}").contains("nonzero size"), "{error:#}");
    }

    #[test]
    fn rejects_duplicate_paths() {
        let mut entries = real_shape();
        entries.push(Fixture {
            path: "hello/2.12.1/bin/hello",
            kind: FixtureKind::File,
            data: b"shadow",
            mode: 0o755,
        });
        let staged = stage(&gzipped(&entries));
        let error = unpack_fixture(&staged).expect_err("duplicate must fail");
        assert!(format!("{error:#}").contains("duplicate"), "{error:#}");
    }

    #[test]
    fn rejects_oversized_and_deep_entries() {
        let bytes = raw_gzipped(&[RawEntry {
            path: b"hello/2.12.1/huge",
            kind: EntryType::Regular,
            size: MAX_FILE_BYTES + 1,
            mode: 0o644,
            data: b"",
        }]);
        let staged = stage(&bytes);
        let error = unpack_fixture(&staged).expect_err("oversized entry must fail");
        assert!(format!("{error:#}").contains("byte limit"), "{error:#}");

        let mut deep = vec![Fixture {
            path: "hello/2.12.1/",
            kind: FixtureKind::Dir,
            data: b"",
            mode: 0o755,
        }];
        let mut path = String::from("hello/2.12.1");
        for index in 0..=MAX_DEPTH {
            path.push_str(&format!("/d{index}"));
            deep.push(Fixture {
                path: Box::leak(path.clone().into_boxed_str()),
                kind: FixtureKind::Dir,
                data: b"",
                mode: 0o755,
            });
        }
        let staged = stage(&gzipped(&deep));
        let error = unpack_fixture(&staged).expect_err("deep entry must fail");
        assert!(format!("{error:#}").contains("depth"), "{error:#}");
    }

    #[test]
    fn limited_reader_enforces_the_expanded_byte_budget() {
        let mut reader = LimitedReader::new(Cursor::new(vec![0_u8; 8]), 4);
        let mut output = Vec::new();
        let error = reader
            .read_to_end(&mut output)
            .expect_err("over-budget input must fail");
        assert!(error.to_string().contains("decompressed byte limit"));
        assert_eq!(output.len(), 4);
    }
}
