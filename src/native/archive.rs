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
//! accepted only when their target (and any symlink hops along it, up to a
//! bounded depth) resolves lexically inside the keg and names a regular file or
//! directory staged in the archive. Link creation is deferred until every
//! regular file and directory is written and the gzip stream has been fully
//! verified, so extraction never traverses an archive link. Absolute, escaping,
//! cyclic, over-deep, and broken links are rejected,
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
/// Maximum number of symlink hops allowed when resolving a staged symlink target.
const MAX_SYMLINK_HOPS: usize = 8;
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
    unpack_layout(archive, out, name, Some(version), sha256)
}

/// Extract a verified standalone release rooted at one exact directory.
pub(super) fn unpack_release(archive: &Path, out: &Path, root: &str, sha256: &str) -> Result<()> {
    validate_digest(sha256)?;
    validate_name(root)?;
    unpack_layout(archive, out, root, None, sha256)
}

fn unpack_layout(
    archive: &Path,
    out: &Path,
    name: &str,
    version: Option<&str>,
    sha256: &str,
) -> Result<()> {
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

/// A symlink entry buffered during tar streaming and validated once every
/// entry in the archive is known.
struct RawSymlink {
    path: PathBuf,
    target_bytes: Vec<u8>,
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
/// Validation runs after every tar entry is seen so forward-referenced
/// keg-internal symlinks (such as `bin/go -> ../libexec/bin/go`) succeed.
fn extract<R: Read>(
    tar: &mut tar::Archive<R>,
    out: &Path,
    name: &str,
    version: Option<&str>,
) -> Result<Vec<PendingSymlink>> {
    let mut seen: HashMap<PathBuf, Seen> = HashMap::new();
    let mut directories: HashSet<PathBuf> = HashSet::new();
    let mut raw_symlinks: Vec<RawSymlink> = Vec::new();
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
            if target_bytes.len() > MAX_LINK_TARGET_BYTES {
                bail!(
                    "bottle archive symlink '{}' has a target longer than {MAX_LINK_TARGET_BYTES} bytes",
                    relative.display()
                );
            }
            if raw_symlinks.len() >= MAX_SYMLINKS {
                bail!("bottle archive has more than {MAX_SYMLINKS} symlinks");
            }
            raw_symlinks.push(RawSymlink {
                path: relative,
                target_bytes: target_bytes.into_owned(),
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

    let mut parsed = Vec::with_capacity(raw_symlinks.len());
    let mut raw_targets = HashMap::with_capacity(raw_symlinks.len());
    for raw in raw_symlinks {
        if directories.contains(&raw.path) {
            bail!(
                "bottle archive symlink '{}' is used as an ancestor directory",
                raw.path.display()
            );
        }
        let immediate = parse_symlink_target(&raw.path, &raw.target_bytes)?;
        raw_targets.insert(raw.path.clone(), immediate.clone());
        parsed.push((raw.path, immediate));
    }

    let mut symlinks = Vec::with_capacity(parsed.len());
    for (path, immediate) in parsed {
        validate_symlink_chain(&path, &immediate, &seen, &directories, &raw_targets)?;
        symlinks.push(PendingSymlink {
            path,
            target: immediate,
        });
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

/// Validate a symlink target lexically and return its immediate keg-relative target.
///
/// Only relative targets that stay inside the keg are accepted. `..` may only
/// appear before any normal component, so a target can never rely on a
/// directory that a later `..` cancels. Chain, cycle, and existence checks run
/// separately in [`validate_symlink_chain`] once all archive symlinks are known.
fn parse_symlink_target(link: &Path, bytes: &[u8]) -> Result<PathBuf> {
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

    let mut immediate = PathBuf::new();
    for component in resolved {
        immediate.push(component);
    }
    Ok(immediate)
}

/// Resolve an immediate keg-relative symlink target purely in-memory across
/// bounded symlink hops, ensuring it is acyclic and ends at a staged regular
/// file or non-ancestor directory.
fn validate_symlink_chain(
    link: &Path,
    immediate: &Path,
    seen: &HashMap<PathBuf, Seen>,
    directories: &HashSet<PathBuf>,
    raw_targets: &HashMap<PathBuf, PathBuf>,
) -> Result<()> {
    let mut current = immediate.to_path_buf();
    let mut hops = 0usize;
    let mut visited = HashSet::new();
    visited.insert(link.to_path_buf());

    'walk: loop {
        let components: Vec<PathBuf> = current
            .components()
            .map(|component| PathBuf::from(component.as_os_str()))
            .collect();
        let mut prefix = PathBuf::new();
        for (index, component) in components.iter().enumerate() {
            prefix.push(component);
            let is_leaf = index + 1 == components.len();
            match seen.get(&prefix) {
                Some(Seen::File) if is_leaf => return Ok(()),
                Some(Seen::File) => bail!(
                    "bottle archive symlink '{}' targets a path through a regular file",
                    link.display()
                ),
                Some(Seen::Dir) => {
                    if is_leaf {
                        // A directory link to one of its own ancestors can be
                        // followed forever (for example `lib/sub/link -> ..`),
                        // so it is not a simple link.
                        if link.starts_with(&prefix) {
                            bail!(
                                "bottle archive symlink '{}' targets an ancestor directory, which would form a cycle",
                                link.display()
                            );
                        }
                        return Ok(());
                    }
                }
                Some(Seen::Symlink) => {
                    if !visited.insert(prefix.clone()) {
                        bail!(
                            "bottle archive symlink '{}' forms a symlink cycle",
                            link.display()
                        );
                    }
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        bail!(
                            "bottle archive symlink '{}' exceeds the symlink resolution depth limit of {MAX_SYMLINK_HOPS}",
                            link.display()
                        );
                    }
                    let expanded_base = raw_targets.get(&prefix).with_context(|| {
                        format!(
                            "bottle archive symlink '{}' references unresolved symlink '{}'",
                            link.display(),
                            prefix.display()
                        )
                    })?;
                    let mut expanded = expanded_base.clone();
                    for tail in &components[index + 1..] {
                        expanded.push(tail);
                    }
                    if expanded.components().count() > MAX_DEPTH {
                        bail!(
                            "bottle archive symlink '{}' has a target that exceeds the depth limit of {MAX_DEPTH}",
                            link.display()
                        );
                    }
                    current = expanded;
                    continue 'walk;
                }
                None if directories.contains(&prefix) => {
                    if is_leaf {
                        if link.starts_with(&prefix) {
                            bail!(
                                "bottle archive symlink '{}' targets an ancestor directory, which would form a cycle",
                                link.display()
                            );
                        }
                        return Ok(());
                    }
                }
                None => bail!(
                    "bottle archive symlink '{}' targets '{}', which is not staged in the archive",
                    link.display(),
                    prefix.display()
                ),
            }
        }
        bail!(
            "bottle archive symlink '{}' resolves to an empty path",
            link.display()
        );
    }
}

/// Create the deferred symlinks after all regular entries are in place.
fn materialize_symlinks(out: &Path, symlinks: &[PendingSymlink]) -> Result<()> {
    for link in symlinks {
        if let Some(parent) = link.path.parent()
            && !parent.as_os_str().is_empty()
        {
            create_directories(out, parent)?;
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
fn relative_path(
    bytes: &[u8],
    name: &str,
    version: Option<&str>,
    is_dir: bool,
) -> Result<Option<PathBuf>> {
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
    let root_depth = if let Some(version) = version {
        if components.len() > 1 && components[1] != version {
            bail!("bottle archive path '{path}' is outside '{name}/{version}/'");
        }
        2
    } else {
        1
    };
    if components.len() <= root_depth {
        if !is_dir {
            bail!("bottle archive path '{path}' is not a directory");
        }
        return Ok(None);
    }
    if trailing_slash && !is_dir {
        bail!("bottle archive file path '{path}' ends with a slash");
    }
    let relative = components[root_depth..].iter().collect();
    Ok(Some(relative))
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
mod tests;
