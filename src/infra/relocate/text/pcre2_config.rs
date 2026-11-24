//! The audited `bin/pcre2-config` handler.

use anyhow::{Context, Result, bail};

use super::super::paths::Paths;
use super::mapping::{
    checked_literal, map_complete_keg_root, map_fixed_cellar, map_keg, shell_quote,
};
use super::{MARKER, contains};

/// Which pcre2-config expansion a known `echo` line performs.
#[derive(Clone, Copy)]
enum EchoKind {
    Prefix,
    ExecPrefix,
    Cflags,
    CflagsPosix,
    LibsPosix,
    Libs8,
    Libs16,
    Libs32,
}

/// Exact upstream `echo` lines and their quoted replacements.
///
/// Only the expansion is quoted; every other byte of the line is preserved.
/// No-space output is byte-for-byte unchanged.
const ECHO_REWRITES: &[(&str, &str, EchoKind)] = &[
    (
        "      echo $prefix\n",
        "      echo \"$prefix\"\n",
        EchoKind::Prefix,
    ),
    (
        "      echo $exec_prefix\n",
        "      echo \"$exec_prefix\"\n",
        EchoKind::ExecPrefix,
    ),
    (
        "      echo $includes \n",
        "      echo \"$includes\" \n",
        EchoKind::Cflags,
    ),
    (
        "        echo $includes -DPCRE2POSIX_SHARED\n",
        "        echo \"$includes\" -DPCRE2POSIX_SHARED\n",
        EchoKind::CflagsPosix,
    ),
    (
        "        echo $libS$libR -lpcre2-posix -lpcre2-8\n",
        "        echo \"$libS$libR\" -lpcre2-posix -lpcre2-8\n",
        EchoKind::LibsPosix,
    ),
    (
        "        echo $libS$libR -lpcre2-8\n",
        "        echo \"$libS$libR\" -lpcre2-8\n",
        EchoKind::Libs8,
    ),
    (
        "        echo $libS$libR -lpcre2-16\n",
        "        echo \"$libS$libR\" -lpcre2-16\n",
        EchoKind::Libs16,
    ),
    (
        "        echo $libS$libR -lpcre2-32\n",
        "        echo \"$libS$libR\" -lpcre2-32\n",
        EchoKind::Libs32,
    ),
];

/// Exact upstream `test` lines whose possibly space-bearing operand must be
/// quoted, or the `if` never assigns `includes`.
const TEST_REWRITES: &[(&str, &str)] = &[
    (
        "      if test ${prefix}/include != /usr/include ; then\n",
        "      if test \"${prefix}/include\" != /usr/include ; then\n",
    ),
    (
        "        if test ${prefix}/include != /usr/include ; then\n",
        "        if test \"${prefix}/include\" != /usr/include ; then\n",
    ),
];

/// Comparisons that must never survive with an unquoted prefix operand.
const UNSAFE_TESTS: &[&str] = &["test ${prefix}/include"];

/// Expansions that must never survive unquoted in pcre2-config.
const UNSAFE_ECHOES: &[&str] = &[
    "echo $prefix",
    "echo $exec_prefix",
    "echo $includes",
    "echo $libS",
    "echo $libs",
];

/// Rewrite the upstream pcre2-config script without touching unknown lines.
pub(super) fn rewrite_pcre2_config(text: &str, paths: &Paths<'_>) -> Result<String> {
    if !text.starts_with("#!/bin/sh\n") {
        bail!("pcre2-config must start with the upstream #!/bin/sh line");
    }
    if !text.is_ascii() {
        bail!("pcre2-config contains non-ASCII bytes");
    }

    let mut out = String::with_capacity(text.len() + 256);
    let mut state = ConfigState::default();

    for line in text.split_inclusive('\n') {
        if contains(line.as_bytes(), MARKER.as_bytes()) {
            out.push_str(&rewrite_config_marker_line(line, paths, &mut state)?);
        } else if line.contains("/opt/homebrew/") {
            out.push_str(&rewrite_config_fixed_line(line, paths, &mut state)?);
        } else if let Some(replacement) = known_test(line) {
            state.cflags_test = true;
            out.push_str(replacement);
        } else if let Some((replacement, kind)) = known_echo(line) {
            state.mark_echo(kind)?;
            out.push_str(replacement);
        } else {
            if UNSAFE_TESTS.iter().any(|needle| line.contains(needle)) {
                bail!("Unsupported pcre2-config test line {line:?}");
            }
            if UNSAFE_ECHOES.iter().any(|needle| line.contains(needle)) {
                bail!("Unsupported pcre2-config echo line {line:?}");
            }
            out.push_str(line);
        }
    }

    state.validate()?;
    if out.contains("/opt/homebrew/") {
        bail!("pcre2-config still contains a fixed /opt/homebrew path");
    }
    Ok(out)
}

/// The marker-bearing lines pcre2-config is allowed to contain.
fn rewrite_config_marker_line(
    line: &str,
    paths: &Paths<'_>,
    state: &mut ConfigState,
) -> Result<String> {
    let body = line
        .strip_suffix('\n')
        .context("pcre2-config line does not end with a newline")?;

    if let Some(value) = body.strip_prefix("prefix=") {
        if state.prefix_assignment {
            bail!("duplicate pcre2-config prefix assignment");
        }
        let mapped = map_complete_keg_root(paths, value)?;
        state.prefix_assignment = true;
        return Ok(format!("prefix={}\n", shell_quote(&mapped)?));
    }

    if let Some(value) = body
        .strip_prefix("case \"")
        .and_then(|value| value.strip_suffix("\" in"))
    {
        if state.case_word {
            bail!("duplicate pcre2-config libS case word");
        }
        let mapped = map_keg(
            paths,
            checked_literal(value, "pcre2-config libS case word")?,
        )?;
        state.case_word = true;
        return Ok(format!("case {} in\n", shell_quote(&mapped)?));
    }

    if let Some(value) = body
        .strip_prefix("  libS=\"-L")
        .and_then(|value| value.strip_suffix('"'))
    {
        if state.libs_assignment {
            bail!("duplicate pcre2-config libS assignment");
        }
        let mapped = map_keg(
            paths,
            checked_literal(value, "pcre2-config libS assignment")?,
        )?;
        state.libs_assignment = true;
        return Ok(format!("  libS={}\n", shell_quote(&format!("-L{mapped}"))?));
    }

    bail!("Unsupported Homebrew placeholder in pcre2-config line {body:?}");
}

/// The two fixed `libR` lines pcre2-config is allowed to contain.
fn rewrite_config_fixed_line(
    line: &str,
    paths: &Paths<'_>,
    state: &mut ConfigState,
) -> Result<String> {
    let body = line
        .strip_suffix('\n')
        .context("pcre2-config line does not end with a newline")?;

    if let Some(value) = body
        .strip_prefix("  libR=\" -R")
        .and_then(|value| value.strip_suffix('"'))
    {
        if state.sunos_lib_r {
            bail!("duplicate pcre2-config *SunOS* libR assignment");
        }
        let mapped = map_fixed_cellar(paths, value)?;
        state.sunos_lib_r = true;
        return Ok(format!(
            "  libR={}\n",
            shell_quote(&format!(" -R{mapped}"))?
        ));
    }

    if let Some(value) = body
        .strip_prefix("  libR=\" -Wl,-R")
        .and_then(|value| value.strip_suffix('"'))
    {
        if state.bsd_lib_r {
            bail!("duplicate pcre2-config *BSD* libR assignment");
        }
        let mapped = map_fixed_cellar(paths, value)?;
        state.bsd_lib_r = true;
        return Ok(format!(
            "  libR={}\n",
            shell_quote(&format!(" -Wl,-R{mapped}"))?
        ));
    }

    bail!("Unsupported fixed /opt/homebrew path in pcre2-config line {body:?}");
}

/// Match one audited upstream `test ${prefix}/include` line.
fn known_test(line: &str) -> Option<&'static str> {
    TEST_REWRITES
        .iter()
        .find_map(|(old, new)| (*old == line).then_some(*new))
}

/// Match one audited upstream `echo` line.
fn known_echo(line: &str) -> Option<(&'static str, EchoKind)> {
    ECHO_REWRITES
        .iter()
        .find_map(|(old, new, kind)| (*old == line).then_some((*new, *kind)))
}

/// Required and duplicated pcre2-config lines observed so far.
#[derive(Default)]
struct ConfigState {
    prefix_assignment: bool,
    case_word: bool,
    libs_assignment: bool,
    prefix_echo: bool,
    exec_prefix_echo: bool,
    cflags_test: bool,
    cflags_echo: bool,
    cflags_posix_echo: bool,
    libs_posix_echo: bool,
    libs8_echo: bool,
    libs16_echo: bool,
    libs32_echo: bool,
    sunos_lib_r: bool,
    bsd_lib_r: bool,
}

impl ConfigState {
    fn mark_echo(&mut self, kind: EchoKind) -> Result<()> {
        let slot = match kind {
            EchoKind::Prefix => &mut self.prefix_echo,
            EchoKind::ExecPrefix => &mut self.exec_prefix_echo,
            EchoKind::Cflags => &mut self.cflags_echo,
            EchoKind::CflagsPosix => &mut self.cflags_posix_echo,
            EchoKind::LibsPosix => &mut self.libs_posix_echo,
            EchoKind::Libs8 => &mut self.libs8_echo,
            EchoKind::Libs16 => &mut self.libs16_echo,
            EchoKind::Libs32 => &mut self.libs32_echo,
        };
        if *slot {
            bail!("duplicate pcre2-config echo line");
        }
        *slot = true;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if !self.prefix_assignment {
            bail!("pcre2-config is missing its pinned prefix assignment");
        }
        if !self.case_word {
            bail!("pcre2-config is missing its libS case word");
        }
        if !self.libs_assignment {
            bail!("pcre2-config is missing its libS assignment");
        }
        if !self.prefix_echo {
            bail!("pcre2-config is missing its --prefix echo");
        }
        if !self.exec_prefix_echo {
            bail!("pcre2-config is missing its --exec-prefix echo");
        }
        if !self.cflags_test {
            bail!("pcre2-config is missing its --cflags test");
        }
        if !self.cflags_echo {
            bail!("pcre2-config is missing its --cflags echo");
        }
        if !self.libs8_echo {
            bail!("pcre2-config is missing its --libs8 echo");
        }
        if self.sunos_lib_r != self.bsd_lib_r {
            bail!("pcre2-config has only one of its two fixed-prefix libR branches");
        }
        Ok(())
    }
}
