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
            no_color: std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
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
