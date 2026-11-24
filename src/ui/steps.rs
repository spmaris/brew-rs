//! Per-step progress for install and upgrade runs.
//!
//! Everything here writes to stderr so stdout stays clean for pipes. On an
//! interactive stderr an animated spinner marks the active step (coloured
//! unless `NO_COLOR` or `ui.colored = false`); otherwise only the finished
//! line is printed.

use colored::Colorize;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::time::Duration;

use crate::config::Config;
use crate::ui::table::Mode;

/// How steps are drawn; `colored` implies `animate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Look {
    animate: bool,
    colored: bool,
}

impl Look {
    fn from_mode(mode: Mode) -> Self {
        // `boxed` is the interactive-terminal decision (TTY and not dumb).
        Self {
            animate: mode.boxed,
            colored: mode.colored,
        }
    }
}

pub struct Steps {
    look: Look,
}

impl Steps {
    pub fn new(config: &Config) -> Self {
        Self {
            look: Look::from_mode(Mode::detect_stderr(config.ui.colored)),
        }
    }

    pub fn start(&self, active: &str) -> Step {
        let bar = if self.look.animate {
            let template = if self.look.colored {
                "  {spinner:.cyan} {msg}"
            } else {
                "  {spinner} {msg}"
            };
            let bar = ProgressBar::new_spinner();
            bar.set_draw_target(ProgressDrawTarget::stderr());
            bar.set_style(
                ProgressStyle::with_template(template)
                    .unwrap_or_else(|_| ProgressStyle::default_spinner())
                    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
            );
            bar.enable_steady_tick(Duration::from_millis(80));
            bar.set_message(active.to_string());
            Some(bar)
        } else {
            None
        };
        Step {
            bar,
            colored: self.look.colored,
        }
    }
}

pub struct Step {
    bar: Option<ProgressBar>,
    colored: bool,
}

impl Step {
    pub fn done(self, finished: &str) {
        if let Some(bar) = self.bar {
            bar.finish_and_clear();
        }
        if self.colored {
            eprintln!("  {} {finished}", "✔".green().bold());
        } else {
            eprintln!("{}", plain_line(finished));
        }
    }

    /// Clear the spinner without printing; the caller reports the error.
    pub fn fail(self) {
        if let Some(bar) = self.bar {
            bar.finish_and_clear();
        }
    }
}

pub fn plain_line(finished: &str) -> String {
    format!("  ✔ {finished}")
}

#[cfg(test)]
mod tests;
