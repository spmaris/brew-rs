use super::*;
use crate::infra::store::{Dependency, Receipt};
use std::fs;
use std::process::Command;

const SPACE_PREFIX: &str = "/home/example/Application Support/brewrs";

const PCRE2_CONFIG: &str = concat!(
    r#"#!/bin/sh

prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49
exec_prefix=${prefix}
exec_prefix_set=no

cflags="[--cflags]"
libs=

if test yes = yes ; then
  libs="[--libs16] $libs"
fi

if test yes = yes ; then
  libs="[--libs32] $libs"
fi

if test yes = yes ; then
  libs="[--libs8] [--libs-posix] $libs"
  cflags="$cflags [--cflags-posix]"
fi

usage="Usage: pcre2-config [--prefix] [--exec-prefix] [--version] $libs $cflags"

if test $# -eq 0; then
  echo "${usage}" 1>&2
  exit 1
fi

libR=
case `uname -s` in
  *SunOS*)
  libR=" -R/opt/homebrew/Cellar/pcre2/10.49/lib"
  ;;
  *BSD*)
  libR=" -Wl,-R/opt/homebrew/Cellar/pcre2/10.49/lib"
  ;;
esac

libS=
case "@@HOMEBREW_CELLAR@@/pcre2/10.49/lib" in
  # system locations on Linux, no -L needed
  /usr/lib|/usr/lib32|/usr/lib64|/usr/lib/*-gnu*)
  ;;
  /lib|/lib32|/lib64|/lib/*-gnu*)
  ;;
  # system locations on AIX and Solaris, no -L needed
  /usr/lib/64|/usr/lib/amd*|/usr/lib/sparc*|/usr/lib/ppc*)
  ;;
  *)
  libS="-L@@HOMEBREW_CELLAR@@/pcre2/10.49/lib"
  ;;
esac

while test $# -gt 0; do
  case "$1" in
  -*=*) optarg=`echo "$1" | sed 's/[-_a-zA-Z0-9]*=//'` ;;
  *) optarg= ;;
  esac

  case $1 in
    --prefix=*)
      prefix=$optarg
      if test $exec_prefix_set = no ; then
        exec_prefix=$optarg
      fi
      ;;
    --prefix)
      echo $prefix
      ;;
    --exec-prefix=*)
      exec_prefix=$optarg
      exec_prefix_set=yes
      ;;
    --exec-prefix)
      echo $exec_prefix
      ;;
    --version)
      echo 10.49
      ;;
    --cflags)
      if test ${prefix}/include != /usr/include ; then
        includes=-I${prefix}/include
      fi
      echo $includes"#,
    " ",
    r#"
      ;;
    --cflags-posix)
      if test yes = yes ; then
        if test ${prefix}/include != /usr/include ; then
          includes=-I${prefix}/include
        fi
        echo $includes -DPCRE2POSIX_SHARED
      else
        echo "${usage}" 1>&2
      fi
      ;;
    --libs-posix)
      if test yes = yes ; then
        echo $libS$libR -lpcre2-posix -lpcre2-8
      else
        echo "${usage}" 1>&2
      fi
      ;;
    --libs8)
      if test yes = yes ; then
        echo $libS$libR -lpcre2-8
      else
        echo "${usage}" 1>&2
      fi
      ;;
    --libs16)
      if test yes = yes ; then
        echo $libS$libR -lpcre2-16
      else
        echo "${usage}" 1>&2
      fi
      ;;
    --libs32)
      if test yes = yes ; then
        echo $libS$libR -lpcre2-32
      else
        echo "${usage}" 1>&2
      fi
      ;;
    *)
      echo "${usage}" 1>&2
      exit 1
      ;;
  esac
  shift
done
"#
);

const PCRE2_PC: &str = concat!(
    r#"# Package Information for pkg-config

prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49
exec_prefix=${prefix}
libdir=@@HOMEBREW_CELLAR@@/pcre2/10.49/lib
includedir=${prefix}/include

Name: libpcre2-8
Description: PCRE2 - Perl compatible regular expressions C library (2nd API) with 8 bit character support
Version: 10.49
License: BSD-3-Clause WITH PCRE2-exception
Libs: -L${libdir} -lpcre2-8
Libs.private: -pthread -lpthread
Cflags: -I${includedir}"#,
    " \n"
);

fn receipt(name: &str, version: &str, dependencies: &[(&str, &str)]) -> Receipt {
    Receipt {
        source: Default::default(),
        name: name.into(),
        version: version.into(),
        version_scheme: 0,
        sha256: "a".repeat(64),
        requested: true,
        keg_only: false,
        conflicts_with: Vec::new(),
        dependencies: dependencies
            .iter()
            .map(|(name, version)| Dependency {
                name: (*name).into(),
                version: (*version).into(),
                sha256: "b".repeat(64),
            })
            .collect(),
    }
}

fn pcre2_receipt() -> Receipt {
    receipt("pcre2", "10.49", &[])
}

fn relocated_at(relative: &str, input: &str, receipt: &Receipt, prefix: &Path) -> String {
    let paths = Paths::new(prefix, receipt).unwrap();
    let output = rewrite(Path::new(relative), input.as_bytes(), &paths)
        .unwrap()
        .expect("fixture must carry a marker");
    String::from_utf8(output).unwrap()
}

fn relocated(relative: &str, input: &str, receipt: &Receipt) -> String {
    relocated_at(relative, input, receipt, Path::new(SPACE_PREFIX))
}

fn failure(relative: &str, input: &[u8], receipt: &Receipt) -> String {
    let paths = Paths::new(Path::new(SPACE_PREFIX), receipt).unwrap();
    format!(
        "{:#}",
        rewrite(Path::new(relative), input, &paths).expect_err("fixture must be rejected")
    )
}

fn run(script: &Path, argument: &str) -> String {
    let output = Command::new("/bin/sh")
        .arg(script)
        .arg(argument)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn pkg_config_rewrites_literal_paths_for_a_space_prefix() {
    let receipt = pcre2_receipt();
    let output = relocated("lib/pkgconfig/libpcre2-8.pc", PCRE2_PC, &receipt);
    assert!(output.contains(&format!("prefix={SPACE_PREFIX}/Cellar/pcre2/10.49\n")));
    assert!(output.contains(&format!("libdir={SPACE_PREFIX}/Cellar/pcre2/10.49/lib\n")));
    assert!(output.contains("exec_prefix=${prefix}\n"));
    assert!(output.contains("includedir=${prefix}/include\n"));
    assert!(output.contains("Libs: -L\"${libdir}\" -lpcre2-8\n"));
    assert!(!output.contains(MARKER));
    assert!(output.ends_with("Cflags: -I\"${includedir}\" \n"));
}

#[test]
fn pkg_config_quotes_path_flags_only_for_space_bearing_variables() {
    let receipt = pcre2_receipt();
    let input = "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\n\
                     libdir=${prefix}/lib\n\
                     Libs: -L${libdir} -L${prefix}/lib -L/usr/lib -lpcre2-8\n";
    let output = relocated("lib/pkgconfig/example.pc", input, &receipt);
    assert!(output.contains("Libs: -L\"${libdir}\" -L\"${prefix}/lib\" -L/usr/lib -lpcre2-8\n"));

    let output = relocated_at(
        "lib/pkgconfig/libpcre2-8.pc",
        PCRE2_PC,
        &receipt,
        Path::new("/tmp/brew-prefix"),
    );
    assert!(output.contains("Libs: -L${libdir} -lpcre2-8\n"));
    assert!(output.ends_with("Cflags: -I${includedir} \n"));
}

#[test]
fn pkg_config_maps_an_opt_placeholder_to_the_pinned_dependency() {
    let receipt = receipt("ripgrep", "16.0.0", &[("pcre2", "10.49")]);
    let input = "prefix=@@HOMEBREW_PREFIX@@/opt/pcre2\nlibdir=${prefix}/lib\n";
    let output = relocated("lib/pkgconfig/libpcre2-8.pc", input, &receipt);
    assert_eq!(
        output,
        format!("prefix={SPACE_PREFIX}/Cellar/pcre2/10.49\nlibdir=${{prefix}}/lib\n")
    );
}

#[test]
fn pkg_config_rejects_unknown_or_misplaced_placeholders() {
    let receipt = pcre2_receipt();
    for input in [
        "prefix=@@HOMEBREW_OPT@@/pcre2/10.49\n",
        "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.48\n",
        "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49/../evil\n",
        "Description: @@HOMEBREW_CELLAR@@/pcre2/10.49\n",
    ] {
        let message = failure("lib/pkgconfig/example.pc", input.as_bytes(), &receipt);
        assert!(
            message.contains("Refusing to rewrite"),
            "{input:?}: {message}"
        );
    }
}

#[test]
fn pkg_config_rejects_duplicates_control_bytes_and_bad_shapes() {
    let receipt = pcre2_receipt();
    let duplicate = "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\n\
                         prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\n";
    assert!(
        failure("lib/pkgconfig/example.pc", duplicate.as_bytes(), &receipt).contains("duplicate")
    );

    let crlf = PCRE2_PC.replace('\n', "\r\n");
    assert!(failure("lib/pkgconfig/example.pc", crlf.as_bytes(), &receipt).contains("control"));

    let mut invalid = PCRE2_PC.as_bytes().to_vec();
    invalid.push(0xff);
    assert!(failure("lib/pkgconfig/example.pc", &invalid, &receipt).contains("UTF-8"));

    assert!(
        failure(
            "lib/pkgconfig/example.pc",
            b"prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49",
            &receipt
        )
        .contains("newline")
    );

    assert!(
        failure("lib/pkgconfig/../example.pc", PCRE2_PC.as_bytes(), &receipt)
            .contains("Unsupported")
    );
    assert!(
        failure(
            "share/pkgconfig/../example.pc",
            PCRE2_PC.as_bytes(),
            &receipt
        )
        .contains("Unsupported")
    );
}

#[test]
fn pcre2_config_rewrites_known_lines_and_executes_with_a_space_prefix() {
    let receipt = pcre2_receipt();
    let output = relocated("bin/pcre2-config", PCRE2_CONFIG, &receipt);
    assert!(output.contains(&format!("prefix='{SPACE_PREFIX}/Cellar/pcre2/10.49'\n")));
    assert!(output.contains("      echo \"$prefix\"\n"));
    assert!(output.contains("      echo \"$exec_prefix\"\n"));
    assert!(output.contains("      echo \"$includes\" \n"));
    assert!(output.contains(&format!(
        "case '{SPACE_PREFIX}/Cellar/pcre2/10.49/lib' in\n"
    )));
    assert!(output.contains(&format!(
        "  libS='-L{SPACE_PREFIX}/Cellar/pcre2/10.49/lib'\n"
    )));
    assert!(!output.contains("/opt/homebrew"));
    assert!(!output.contains(MARKER));

    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("pcre2-config");
    fs::write(&script, output.as_bytes()).unwrap();

    assert_eq!(
        run(&script, "--prefix"),
        format!("{SPACE_PREFIX}/Cellar/pcre2/10.49\n")
    );
    assert_eq!(
        run(&script, "--exec-prefix"),
        format!("{SPACE_PREFIX}/Cellar/pcre2/10.49\n")
    );
    assert_eq!(
        run(&script, "--cflags"),
        format!("-I{SPACE_PREFIX}/Cellar/pcre2/10.49/include\n")
    );
    assert_eq!(
        run(&script, "--libs8"),
        format!("-L{SPACE_PREFIX}/Cellar/pcre2/10.49/lib -lpcre2-8\n")
    );
    assert_eq!(
        run(&script, "--cflags-posix"),
        format!("-I{SPACE_PREFIX}/Cellar/pcre2/10.49/include -DPCRE2POSIX_SHARED\n")
    );
    assert_eq!(
        run(&script, "--libs-posix"),
        format!("-L{SPACE_PREFIX}/Cellar/pcre2/10.49/lib -lpcre2-posix -lpcre2-8\n")
    );
    assert_eq!(
        run(&script, "--libs16"),
        format!("-L{SPACE_PREFIX}/Cellar/pcre2/10.49/lib -lpcre2-16\n")
    );
    assert_eq!(
        run(&script, "--libs32"),
        format!("-L{SPACE_PREFIX}/Cellar/pcre2/10.49/lib -lpcre2-32\n")
    );
    assert_eq!(run(&script, "--version"), "10.49\n");
}

#[test]
fn pcre2_config_rewrites_the_inert_sunos_and_bsd_branches() {
    let receipt = pcre2_receipt();
    let output = relocated("bin/pcre2-config", PCRE2_CONFIG, &receipt);
    assert!(output.contains(&format!(
        "  libR=' -R{SPACE_PREFIX}/Cellar/pcre2/10.49/lib'\n"
    )));
    assert!(output.contains(&format!(
        "  libR=' -Wl,-R{SPACE_PREFIX}/Cellar/pcre2/10.49/lib'\n"
    )));
}

#[test]
fn pcre2_config_rejects_unknown_placeholder_and_fixed_path_contexts() {
    let receipt = pcre2_receipt();

    let unknown = PCRE2_CONFIG.replace(
        "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\n",
        "prefix=@@HOMEBREW_OPT@@/pcre2/10.49\n",
    );
    assert!(failure("bin/pcre2-config", unknown.as_bytes(), &receipt).contains("placeholder"));

    let wrong_version = PCRE2_CONFIG.replace(
        "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\n",
        "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.48\n",
    );
    assert!(failure("bin/pcre2-config", wrong_version.as_bytes(), &receipt).contains("10.48"));

    let fixed_elsewhere = PCRE2_CONFIG.replace(
        "      echo 10.49\n",
        "      echo /opt/homebrew/Cellar/pcre2/10.49/lib\n",
    );
    assert!(
        failure("bin/pcre2-config", fixed_elsewhere.as_bytes(), &receipt).contains("/opt/homebrew")
    );

    let duplicate = PCRE2_CONFIG.replace(
        "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\n",
        "prefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\nprefix=@@HOMEBREW_CELLAR@@/pcre2/10.49\n",
    );
    assert!(failure("bin/pcre2-config", duplicate.as_bytes(), &receipt).contains("duplicate"));
}

#[test]
fn pcre2_config_rejects_unquoted_or_missing_known_lines() {
    let receipt = pcre2_receipt();

    let trailing_space = PCRE2_CONFIG.replace("      echo $prefix\n", "      echo $prefix \n");
    assert!(failure("bin/pcre2-config", trailing_space.as_bytes(), &receipt).contains("echo"));

    let missing_prefix_echo = PCRE2_CONFIG.replace("      echo $prefix\n", "");
    assert!(
        failure("bin/pcre2-config", missing_prefix_echo.as_bytes(), &receipt).contains("--prefix")
    );

    let missing_libs8_echo = PCRE2_CONFIG.replace("        echo $libS$libR -lpcre2-8\n", "");
    assert!(
        failure("bin/pcre2-config", missing_libs8_echo.as_bytes(), &receipt).contains("--libs8")
    );

    let missing_case_word =
        PCRE2_CONFIG.replace("case \"@@HOMEBREW_CELLAR@@/pcre2/10.49/lib\" in\n", "");
    assert!(failure("bin/pcre2-config", missing_case_word.as_bytes(), &receipt).contains("case"));
}

#[test]
fn pcre2_config_rejects_control_bytes_and_wrong_shebang() {
    let receipt = pcre2_receipt();

    let crlf = PCRE2_CONFIG.replace('\n', "\r\n");
    assert!(failure("bin/pcre2-config", crlf.as_bytes(), &receipt).contains("control"));

    let wrong_shebang = PCRE2_CONFIG.replace("#!/bin/sh", "#!/bin/bash");
    assert!(failure("bin/pcre2-config", wrong_shebang.as_bytes(), &receipt).contains("#!/bin/sh"));

    let mut invalid = PCRE2_CONFIG.as_bytes().to_vec();
    invalid.push(0xff);
    assert!(failure("bin/pcre2-config", &invalid, &receipt).contains("UTF-8"));
}

#[test]
fn arbitrary_data_without_a_marker_is_not_rewritten() {
    let receipt = pcre2_receipt();
    let paths = Paths::new(Path::new(SPACE_PREFIX), &receipt).unwrap();
    for (relative, bytes) in [
        ("bin/pcre2-config", b"not a supported script".as_slice()),
        (
            "lib/pkgconfig/example.pc",
            b"prefix=/usr/local\n".as_slice(),
        ),
        ("share/doc/readme", b"hello\n".as_slice()),
        ("share/doc/readme", b"\xff\xfe".as_slice()),
    ] {
        assert_eq!(
            rewrite(Path::new(relative), bytes, &paths).unwrap(),
            None,
            "{relative}"
        );
    }
}

#[test]
fn rejects_non_text_and_unknown_or_unpinned_markers_in_general_text() {
    let aom = receipt("aom", "3.15.3", &[("libvmaf", "3.2.1")]);
    for (rel, bytes, expected) in [
        (
            "opaque.dat",
            b"\0@@HOMEBREW_PREFIX@@/lib\n".as_slice(),
            "control",
        ),
        (
            "bin/bad",
            b"\xff@@HOMEBREW_PREFIX@@/lib\n".as_slice(),
            "UTF-8",
        ),
        (
            "bin/tool-config",
            b"perl=@@HOMEBREW_PERL@@/bin/perl\n".as_slice(),
            "Unsupported Homebrew placeholder",
        ),
        (
            "bin/tool-config",
            b"dir=@@HOMEBREW_CELLAR@@\n".as_slice(),
            "Unsupported Homebrew placeholder",
        ),
        (
            "bin/tool-config",
            b"dir=@@HOMEBREW_PREFIX@@/opt\n".as_slice(),
            "without formula name",
        ),
        (
            "bin/tool-config",
            b"ssl=@@HOMEBREW_PREFIX@@/opt/openssl@4/lib\n".as_slice(),
            "undeclared keg",
        ),
        (
            "lib/cmake/AOMTargets.cmake",
            b"\"-L@@HOMEBREW_CELLAR@@/libvmaf/1.0.0/lib\"\n".as_slice(),
            "pins \"3.2.1\"",
        ),
        (
            "lib/cmake/AOMTargets.cmake",
            b"\"-L@@HOMEBREW_CELLAR@@/aom/3.15.3/../evil/lib\"\n".as_slice(),
            "traversal",
        ),
    ] {
        let message = failure(rel, bytes, &aom);
        assert!(
            message.contains(expected),
            "{rel}: expected {expected:?}, got {message}"
        );
    }
}

#[test]
fn relocates_surveyed_pc_and_general_text_shapes() {
    let xorg_receipt = receipt("xorgproto", "2026.1", &[]);
    let xproto = relocated(
        "share/pkgconfig/xproto.pc",
        "prefix=@@HOMEBREW_CELLAR@@/xorgproto/2026.1\nincludedir=${prefix}/include\nCflags: -I${includedir}\n",
        &xorg_receipt,
    );
    assert_eq!(
        xproto,
        format!(
            "prefix={SPACE_PREFIX}/Cellar/xorgproto/2026.1\nincludedir=${{prefix}}/include\nCflags: -I\"${{includedir}}\"\n"
        )
    );

    let icu_receipt = receipt("icu4c@79", "79.1", &[]);
    let icu_pc = relocated(
        "lib/pkgconfig/icu-uc.pc",
        "prefix = @@HOMEBREW_PREFIX@@/opt/icu4c@79\nlibdir = @@HOMEBREW_PREFIX@@/opt/icu4c@79/lib\n#ICUPKGDATA_DIR=@@HOMEBREW_PREFIX@@/opt/icu4c@79/lib\nLibs: -L${libdir} -licuuc\n",
        &icu_receipt,
    );
    assert!(icu_pc.contains(&format!("prefix = {SPACE_PREFIX}/Cellar/icu4c@79/79.1\n")));
    assert!(icu_pc.contains(&format!(
        "#ICUPKGDATA_DIR={SPACE_PREFIX}/Cellar/icu4c@79/79.1/lib\n"
    )));
    assert!(icu_pc.contains("Libs: -L\"${libdir}\" -licuuc\n"));

    let icu_inc = relocated(
        "lib/icu/79.1/Makefile.inc",
        "prefix = @@HOMEBREW_CELLAR@@/icu4c@79/79.1\n\tlibdir = @@HOMEBREW_CELLAR@@/icu4c@79/79.1/lib\n",
        &icu_receipt,
    );
    assert!(icu_inc.contains(&format!("prefix = {SPACE_PREFIX}/Cellar/icu4c@79/79.1\n")));
    assert!(icu_inc.contains(&format!(
        "\tlibdir = {SPACE_PREFIX}/Cellar/icu4c@79/79.1/lib\n"
    )));
    let icu_pkgdata = relocated(
        "lib/icu/79.1/pkgdata.inc",
        "LIBFLAGS=-I@@HOMEBREW_CELLAR@@/icu4c@79/79.1/include  -dynamic\n",
        &icu_receipt,
    );
    assert_eq!(
        icu_pkgdata,
        format!("LIBFLAGS=-I{SPACE_PREFIX}/Cellar/icu4c@79/79.1/include  -dynamic\n")
    );
    let icu_config = relocated(
        "bin/icu-config",
        "default_prefix=\"@@HOMEBREW_PREFIX@@/opt/icu4c@79\"\n\tlibdir=\"@@HOMEBREW_PREFIX@@/opt/icu4c@79/lib\"\n",
        &icu_receipt,
    );
    assert!(icu_config.contains(&format!(
        "default_prefix=\"{SPACE_PREFIX}/Cellar/icu4c@79/79.1\"\n"
    )));

    let freetype_receipt = receipt(
        "freetype",
        "2.15.0",
        &[("brotli", "1.2.1"), ("libpng", "1.6.59")],
    );
    let ft_pc_input = "Libs: -L@@HOMEBREW_PREFIX@@/opt/freetype/lib -lfreetype\nCflags: -I@@HOMEBREW_PREFIX@@/opt/freetype/include/freetype2\n";
    let ft_pc_space = relocated("lib/pkgconfig/freetype2.pc", ft_pc_input, &freetype_receipt);
    assert_eq!(
        ft_pc_space,
        format!(
            "Libs: -L\"{SPACE_PREFIX}/Cellar/freetype/2.15.0/lib\" -lfreetype\nCflags: -I\"{SPACE_PREFIX}/Cellar/freetype/2.15.0/include/freetype2\"\n"
        )
    );
    let ft_pc_nospace = relocated_at(
        "lib/pkgconfig/freetype2.pc",
        ft_pc_input,
        &freetype_receipt,
        Path::new("/tmp/brew"),
    );
    assert_eq!(
        ft_pc_nospace,
        "Libs: -L/tmp/brew/Cellar/freetype/2.15.0/lib -lfreetype\nCflags: -I/tmp/brew/Cellar/freetype/2.15.0/include/freetype2\n"
    );
    let ft_cfg = relocated(
        "bin/freetype-config",
        "\tprefix=\"@@HOMEBREW_PREFIX@@/opt/freetype\"\n  static_libs=\"-lfreetype -L@@HOMEBREW_PREFIX@@/opt/libpng/lib -lpng16 -L@@HOMEBREW_CELLAR@@/brotli/1.2.1/lib -lbrotlidec\"\n",
        &freetype_receipt,
    );
    assert!(ft_cfg.contains(&format!("-L{SPACE_PREFIX}/Cellar/libpng/1.6.59/lib")));
    assert!(ft_cfg.contains(&format!("-L{SPACE_PREFIX}/Cellar/brotli/1.2.1/lib")));

    let gr2_receipt = receipt("graphite2", "1.3.15", &[]);
    let gr2_pc = relocated(
        "lib/pkgconfig/graphite2.pc",
        "Libs: -L@@HOMEBREW_PREFIX@@/opt/graphite2/lib -lgraphite2\nCflags: -I@@HOMEBREW_PREFIX@@/opt/graphite2/include\n",
        &gr2_receipt,
    );
    assert_eq!(
        gr2_pc,
        format!(
            "Libs: -L\"{SPACE_PREFIX}/Cellar/graphite2/1.3.15/lib\" -lgraphite2\nCflags: -I\"{SPACE_PREFIX}/Cellar/graphite2/1.3.15/include\"\n"
        )
    );

    let luajit_receipt = receipt("luajit", "2.1.1795119671", &[]);
    let luajit_pc = relocated(
        "lib/pkgconfig/luajit.pc",
        "abiver=5.1\nprefix=@@HOMEBREW_CELLAR@@/luajit/2.1.1795119671\nINSTALL_LMOD=@@HOMEBREW_PREFIX@@/share/lua/${abiver}\nINSTALL_CMOD=@@HOMEBREW_PREFIX@@/${multilib}/lua/${abiver}\n",
        &luajit_receipt,
    );
    assert!(luajit_pc.contains(&format!(
        "INSTALL_LMOD={SPACE_PREFIX}/share/lua/${{abiver}}\n"
    )));
    assert!(luajit_pc.contains(&format!(
        "INSTALL_CMOD={SPACE_PREFIX}/${{multilib}}/lua/${{abiver}}\n"
    )));

    let aom_receipt = receipt("aom", "3.15.3", &[("libvmaf", "3.2.1")]);
    let cmake = relocated(
        "lib/cmake/AOM/AOMTargets.cmake",
        "  INTERFACE_LINK_LIBRARIES \"-L@@HOMEBREW_CELLAR@@/libvmaf/3.2.1/lib;\\$<LINK_ONLY:-lvmaf>;Threads::Threads\"\n",
        &aom_receipt,
    );
    assert!(cmake.contains(&format!(
        "-L{SPACE_PREFIX}/Cellar/libvmaf/3.2.1/lib;\\$<LINK_ONLY:-lvmaf>"
    )));

    let png_receipt = receipt("libpng", "1.6.59", &[]);
    let png_cfg = relocated(
        "bin/libpng16-config",
        "prefix=\"@@HOMEBREW_CELLAR@@/libpng/1.6.59\"\nlibdir=\"@@HOMEBREW_CELLAR@@/libpng/1.6.59/lib\"\n",
        &png_receipt,
    );
    assert!(png_cfg.contains(&format!(
        "libdir=\"{SPACE_PREFIX}/Cellar/libpng/1.6.59/lib\""
    )));

    let cairo_receipt = receipt("cairo", "1.18.6", &[]);
    let cairo_trace = relocated(
        "bin/cairo-trace",
        "prefix=@@HOMEBREW_CELLAR@@/cairo/1.18.6\n\tfor lib in @@HOMEBREW_CELLAR@@/cairo/1.18.6/lib/cairo/libcairo-trace.dylib* ; do\n\techo \"in @@HOMEBREW_CELLAR@@/cairo/1.18.6/lib/cairo/.\" >&2\n",
        &cairo_receipt,
    );
    assert!(cairo_trace.contains(&format!("prefix={SPACE_PREFIX}/Cellar/cairo/1.18.6\n")));
    assert!(cairo_trace.contains(&format!(
        "{SPACE_PREFIX}/Cellar/cairo/1.18.6/lib/cairo/libcairo-trace.dylib*"
    )));
    assert!(cairo_trace.contains(&format!("{SPACE_PREFIX}/Cellar/cairo/1.18.6/lib/cairo/.")));
}
