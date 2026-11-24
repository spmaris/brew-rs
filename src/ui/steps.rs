//! Per-step progress for install and upgrade runs.
//!
//! Everything here writes to stderr so stdout stays clean for pipes. On a
//! colour-capable stderr an animated spinner marks the active step; otherwise
//! only the finished line is printed.

use colored::Colorize;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::time::Duration;

use crate::config::Config;
use crate::ui::table::Mode;

pub struct Steps {
    styled: bool,
}

impl Steps {
    pub fn new(config: &Config) -> Self {
        Self {
            styled: Mode::detect_stderr(config.ui.colored).colored,
        }
    }

    pub fn start(&self, active: &str) -> Step {
        let bar = if self.styled {
            let bar = ProgressBar::new_spinner();
            bar.set_draw_target(ProgressDrawTarget::stderr());
            bar.set_style(
                ProgressStyle::with_template("  {spinner:.cyan} {msg}")
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
            styled: self.styled,
        }
    }
}

pub struct Step {
    bar: Option<ProgressBar>,
    styled: bool,
}

impl Step {
    pub fn done(self, finished: &str) {
        if let Some(bar) = self.bar {
            bar.finish_and_clear();
        }
        if self.styled {
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
