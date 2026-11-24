//! The colour and box decision, made once from the environment.

/// Everything the mode decision depends on, injectable for tests.
#[derive(Clone, Copy, Debug)]
pub struct ModeInputs {
    pub is_tty: bool,
    pub term_dumb: bool,
    pub no_color: bool,
    pub config_colored: bool,
}

impl ModeInputs {
    fn from_env(is_tty: bool, config_colored: bool) -> Self {
        Self {
            is_tty,
            term_dumb: std::env::var_os("TERM").as_deref() == Some("dumb".as_ref()),
            no_color: Mode::no_color_requested(),
            config_colored,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// Emit ANSI styling.
    pub colored: bool,
    /// Draw rounded panels fitted to the terminal width.
    pub boxed: bool,
}

impl Mode {
    #[cfg(test)]
    pub const PLAIN: Mode = Mode {
        colored: false,
        boxed: false,
    };

    /// A dumb terminal gets neither boxes nor colour; a non-TTY gets neither
    /// either; `NO_COLOR` and `ui.colored = false` only remove colour.
    pub fn decide(inputs: ModeInputs) -> Self {
        let interactive = inputs.is_tty && !inputs.term_dumb;
        Self {
            colored: interactive && !inputs.no_color && inputs.config_colored,
            boxed: interactive,
        }
    }

    /// `NO_COLOR` counts only when present and non-empty (no-color.org).
    pub(crate) fn no_color_requested() -> bool {
        no_color_value(std::env::var_os("NO_COLOR").as_deref())
    }

    pub fn detect(config_colored: bool) -> Self {
        Self::decide(ModeInputs::from_env(
            console::Term::stdout().is_term(),
            config_colored,
        ))
    }

    pub fn detect_stderr(config_colored: bool) -> Self {
        Self::decide(ModeInputs::from_env(
            console::Term::stderr().is_term(),
            config_colored,
        ))
    }
}

fn no_color_value(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_counts_only_when_set_and_non_empty() {
        assert!(!no_color_value(None));
        assert!(!no_color_value(Some("".as_ref())));
        assert!(no_color_value(Some("1".as_ref())));
        assert!(no_color_value(Some("0".as_ref())));
    }
}
