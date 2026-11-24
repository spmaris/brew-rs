use std::io::{self, Cursor, Write};
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tar::EntryType;
use tempfile::tempdir;

use super::limits::{MAX_DEPTH, MAX_FILE_BYTES, MAX_LINK_COMPONENTS, MAX_LINK_TARGET_BYTES};
use super::*;

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

fn unpack_fixture(staged: &Staged) -> Result<(), ArchiveError> {
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
        ("missing", "not staged in the archive"),
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
            "self_cycle",
            vec![Fixture {
                path: "hello/2.12.1/lib/self",
                kind: FixtureKind::Symlink("self"),
                data: b"",
                mode: 0o777,
            }],
            "cycle",
        ),
        (
            "two_cycle",
            vec![
                Fixture {
                    path: "hello/2.12.1/lib/a",
                    kind: FixtureKind::Symlink("b"),
                    data: b"",
                    mode: 0o777,
                },
                Fixture {
                    path: "hello/2.12.1/lib/b",
                    kind: FixtureKind::Symlink("a"),
                    data: b"",
                    mode: 0o777,
                },
            ],
            "cycle",
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

#[cfg(unix)]
#[test]
fn extracts_forward_referenced_contained_relative_symlinks() {
    let entries = vec![
        Fixture {
            path: "hello/2.12.1/bin/go",
            kind: FixtureKind::Symlink("../libexec/bin/go"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/bin/gofmt",
            kind: FixtureKind::Symlink("../libexec/bin/gofmt"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/libexec/bin/go",
            kind: FixtureKind::File,
            data: b"go-binary",
            mode: 0o755,
        },
        Fixture {
            path: "hello/2.12.1/libexec/bin/gofmt",
            kind: FixtureKind::File,
            data: b"gofmt-binary",
            mode: 0o755,
        },
    ];
    let staged = stage(&gzipped(&entries));
    unpack_fixture(&staged).expect("forward-referenced relative links");

    assert_eq!(
        fs::read_link(staged.out.join("bin/go")).expect("go link"),
        Path::new("../libexec/bin/go")
    );
    assert_eq!(
        fs::read(staged.out.join("bin/go")).expect("read go via link"),
        b"go-binary"
    );
    assert_eq!(
        fs::read(staged.out.join("bin/gofmt")).expect("read gofmt via link"),
        b"gofmt-binary"
    );
}

#[cfg(unix)]
#[test]
fn extracts_bounded_chained_and_directory_symlinks() {
    let forward_chain = vec![
        Fixture {
            path: "hello/2.12.1/lib/libfoo.dylib",
            kind: FixtureKind::Symlink("libfoo.1.dylib"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/libfoo.1.dylib",
            kind: FixtureKind::Symlink("libfoo.1.2.dylib"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/libfoo.1.2.dylib",
            kind: FixtureKind::Symlink("libfoo.1.2.3.dylib"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/libfoo.1.2.3.dylib",
            kind: FixtureKind::File,
            data: b"dylib-bytes",
            mode: 0o755,
        },
    ];
    let staged = stage(&gzipped(&forward_chain));
    unpack_fixture(&staged).expect("forward 3-hop chain");
    assert_eq!(
        fs::read_link(staged.out.join("lib/libfoo.dylib")).unwrap(),
        Path::new("libfoo.1.dylib")
    );
    assert_eq!(
        fs::read(staged.out.join("lib/libfoo.dylib")).unwrap(),
        b"dylib-bytes"
    );

    let reverse_chain = vec![
        Fixture {
            path: "hello/2.12.1/lib/libfoo.1.2.3.dylib",
            kind: FixtureKind::File,
            data: b"reverse-bytes",
            mode: 0o755,
        },
        Fixture {
            path: "hello/2.12.1/lib/libfoo.1.dylib",
            kind: FixtureKind::Symlink("libfoo.1.2.3.dylib"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/libfoo.dylib",
            kind: FixtureKind::Symlink("libfoo.1.dylib"),
            data: b"",
            mode: 0o777,
        },
    ];
    let staged = stage(&gzipped(&reverse_chain));
    unpack_fixture(&staged).expect("reverse 2-hop chain");
    assert_eq!(
        fs::read(staged.out.join("lib/libfoo.dylib")).unwrap(),
        b"reverse-bytes"
    );

    let dir_symlink_chain = vec![
        Fixture {
            path: "hello/2.12.1/lib/icu/Makefile.inc",
            kind: FixtureKind::Symlink("current/Makefile.inc"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/icu/current",
            kind: FixtureKind::Symlink("79.1"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/icu/79.1/Makefile.inc",
            kind: FixtureKind::File,
            data: b"icu-makefile",
            mode: 0o644,
        },
    ];
    let staged = stage(&gzipped(&dir_symlink_chain));
    unpack_fixture(&staged).expect("directory symlink traversal");
    assert_eq!(
        fs::read_link(staged.out.join("lib/icu/Makefile.inc")).unwrap(),
        Path::new("current/Makefile.inc")
    );
    assert_eq!(
        fs::read(staged.out.join("lib/icu/Makefile.inc")).unwrap(),
        b"icu-makefile"
    );

    let indirect_ancestor_cycle = vec![
        Fixture {
            path: "hello/2.12.1/lib/sub/dir",
            kind: FixtureKind::Symlink("up"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/sub/up",
            kind: FixtureKind::Symlink(".."),
            data: b"",
            mode: 0o777,
        },
    ];
    let error = unpack_fixture(&stage(&gzipped(&indirect_ancestor_cycle)))
        .expect_err("indirect ancestor cycle must fail");
    assert!(
        format!("{error:#}").contains("ancestor directory"),
        "{error:#}"
    );

    let broken_chain = vec![
        Fixture {
            path: "hello/2.12.1/lib/a",
            kind: FixtureKind::Symlink("b"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/b",
            kind: FixtureKind::Symlink("missing"),
            data: b"",
            mode: 0o777,
        },
    ];
    let error =
        unpack_fixture(&stage(&gzipped(&broken_chain))).expect_err("broken chain must fail");
    assert!(
        format!("{error:#}").contains("not staged in the archive"),
        "{error:#}"
    );

    let deep_chain = vec![
        Fixture {
            path: "hello/2.12.1/lib/l0",
            kind: FixtureKind::Symlink("l1"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l1",
            kind: FixtureKind::Symlink("l2"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l2",
            kind: FixtureKind::Symlink("l3"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l3",
            kind: FixtureKind::Symlink("l4"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l4",
            kind: FixtureKind::Symlink("l5"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l5",
            kind: FixtureKind::Symlink("l6"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l6",
            kind: FixtureKind::Symlink("l7"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l7",
            kind: FixtureKind::Symlink("l8"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l8",
            kind: FixtureKind::Symlink("l9"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/l9",
            kind: FixtureKind::Symlink("real"),
            data: b"",
            mode: 0o777,
        },
        Fixture {
            path: "hello/2.12.1/lib/real",
            kind: FixtureKind::File,
            data: b"x",
            mode: 0o644,
        },
    ];
    let error =
        unpack_fixture(&stage(&gzipped(&deep_chain))).expect_err("over-deep chain must fail");
    assert!(format!("{error:#}").contains("depth limit"), "{error:#}");
}

#[test]
fn path_traversal_is_a_typed_error() {
    let bytes = raw_gzipped(&[RawEntry {
        path: b"hello/2.12.1/../../escape",
        kind: EntryType::Regular,
        size: 1,
        mode: 0o644,
        data: b"x",
    }]);
    let staged = stage(&bytes);
    let error = unpack_fixture(&staged).expect_err("traversal must fail");
    assert!(
        matches!(&error, ArchiveError::PathNotNormalized { path } if path.contains("..")),
        "{error:?}"
    );
}

#[test]
fn symlink_escape_is_a_typed_error() {
    let entries = [
        Fixture {
            path: "hello/2.12.1/lib/real",
            kind: FixtureKind::File,
            data: b"x",
            mode: 0o644,
        },
        Fixture {
            path: "hello/2.12.1/lib/link",
            kind: FixtureKind::Symlink("../../escape"),
            data: b"",
            mode: 0o777,
        },
    ];
    let staged = stage(&gzipped(&entries));
    let error = unpack_fixture(&staged).expect_err("escaping link must fail");
    assert!(
        matches!(&error, ArchiveError::SymlinkEscapesRoot { link } if link.ends_with("lib/link")),
        "{error:?}"
    );
}

#[test]
fn oversized_entry_is_a_typed_error() {
    let bytes = raw_gzipped(&[RawEntry {
        path: b"hello/2.12.1/huge",
        kind: EntryType::Regular,
        size: MAX_FILE_BYTES + 1,
        mode: 0o644,
        data: b"",
    }]);
    let staged = stage(&bytes);
    let error = unpack_fixture(&staged).expect_err("oversized entry must fail");
    assert!(
        matches!(&error, ArchiveError::EntryTooLarge { limit, .. } if *limit == MAX_FILE_BYTES),
        "{error:?}"
    );
}
