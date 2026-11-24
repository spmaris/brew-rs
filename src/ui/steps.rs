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
use crate::domain::progress::DownloadProgress;
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
            let bar = ProgressBar::new_spinner();
            bar.set_draw_target(ProgressDrawTarget::stderr());
            bar.set_style(styled(&spinner_template(self.look.colored)));
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

/// Spinner glyphs shared by every step line.
const TICK_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

fn styled(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_chars(TICK_CHARS)
        .progress_chars("#-")
}

fn spinner_template(colored: bool) -> String {
    let spinner = if colored {
        "{spinner:.cyan}"
    } else {
        "{spinner}"
    };
    format!("  {spinner} {{msg}}")
}

/// Pacman-style line used once the total size is known.
fn bar_template(colored: bool) -> String {
    let (spinner, bar) = if colored {
        ("{spinner:.cyan}", "{wide_bar:.cyan}")
    } else {
        ("{spinner}", "{wide_bar}")
    };
    format!(
        "  {spinner} {{msg}}  {{bytes}}/{{total_bytes}}  [{bar}]  {{percent:>3}}%  {{binary_bytes_per_sec}}  {{eta}}"
    )
}

/// Line used when the server did not announce a size.
fn unsized_template(colored: bool) -> String {
    let spinner = if colored {
        "{spinner:.cyan}"
    } else {
        "{spinner}"
    };
    format!("  {spinner} {{msg}}  {{bytes}}  {{binary_bytes_per_sec}}")
}

/// Drives the step's own bar while bytes stream.
///
/// Holds a clone of the step's bar, so the step still clears it before the
/// finished line, or on failure. Draws nothing when the step is not animated.
pub struct DownloadBar {
    bar: Option<ProgressBar>,
    label: String,
    colored: bool,
}

impl DownloadProgress for DownloadBar {
    fn start(&self, total: Option<u64>) {
        let Some(bar) = &self.bar else {
            return;
        };
        let template = match total {
            Some(_) => bar_template(self.colored),
            None => unsized_template(self.colored),
        };
        bar.set_message(self.label.clone());
        bar.set_style(styled(&template));
        match total {
            Some(total) => bar.set_length(total),
            None => bar.unset_length(),
        }
        bar.reset();
    }

    fn advance(&self, bytes: u64) {
        if let Some(bar) = &self.bar {
            bar.inc(bytes);
        }
    }

    fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

impl Step {
    /// A progress sink that turns this step's spinner into a download bar.
    pub fn download_progress(&self, label: &str) -> DownloadBar {
        DownloadBar {
            bar: self.bar.clone(),
            label: download_label(label),
            colored: self.colored,
        }
    }

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

/// Width the package name is padded to so columns line up across steps.
const LABEL_WIDTH: usize = 24;

fn download_label(name: &str) -> String {
    format!("{name:<LABEL_WIDTH$}")
}

pub fn plain_line(finished: &str) -> String {
    format!("  ✔ {finished}")
}

#[cfg(test)]
mod tests;
