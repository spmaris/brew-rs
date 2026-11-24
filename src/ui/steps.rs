//! Per-step progress for install and upgrade runs.
//!
//! Everything here writes to stderr so stdout stays clean for pipes. On an
//! interactive stderr an animated spinner marks the active step (coloured
//! unless `NO_COLOR` or `ui.colored = false`); otherwise only the finished
//! line is printed.

use colored::Colorize;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use std::fmt::Write;
use std::time::Duration;

use crate::config::Config;
use crate::domain::progress::DownloadProgress;
use crate::ui::table::Mode;
use crate::ui::text::format_size;

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

/// Display columns of the package name; longer names end in `…`.
const LABEL_WIDTH: usize = 20;
/// Columns of every size field (`1016.2 KiB` is 10).
const SIZE_WIDTH: usize = 10;
/// Columns of the `mm:ss` estimate.
const ETA_WIDTH: usize = 5;
const MIN_BAR: usize = 10;
const MAX_BAR: usize = 60;
const FALLBACK_TERMINAL_WIDTH: usize = 80;

/// Which optional trailing fields fit next to the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fields {
    Full,
    NoEta,
    Minimal,
}

impl Fields {
    /// Columns outside the bar: indent, spinner, label, sizes, brackets,
    /// percent and the optional speed and estimate.
    fn fixed_columns(self) -> usize {
        let base = 2 + 1 + 1 + LABEL_WIDTH // indent, spinner, gap, label
            + 2 + SIZE_WIDTH + 3 + SIZE_WIDTH // gap, done / total
            + 2 + 1 + 1 // gap, `[`, `]` (the bar itself is separate)
            + 2 + 3 + 1; // gap, percent, `%`
        let speed = 2 + SIZE_WIDTH + 2;
        let eta = 2 + ETA_WIDTH;
        match self {
            Self::Full => base + speed + eta,
            Self::NoEta => base + speed,
            Self::Minimal => base,
        }
    }
}

/// Bar width fixed for a whole download, with the fields that go beside it.
/// Drops the estimate, then the speed, before letting the bar fall below
/// its minimum.
fn bar_width(terminal_width: usize) -> (usize, Fields) {
    for fields in [Fields::Full, Fields::NoEta] {
        let room = terminal_width.saturating_sub(fields.fixed_columns());
        if room >= MIN_BAR {
            return (room.min(MAX_BAR), fields);
        }
    }
    let room = terminal_width.saturating_sub(Fields::Minimal.fixed_columns());
    (room.clamp(MIN_BAR, MAX_BAR), Fields::Minimal)
}

fn terminal_width() -> usize {
    let (_, cols) = console::Term::stderr().size();
    match usize::from(cols) {
        0 => FALLBACK_TERMINAL_WIDTH,
        cols => cols,
    }
}

/// Name cut to, or padded out to, exactly `LABEL_WIDTH` display columns.
fn download_label(name: &str) -> String {
    let fitted = console::truncate_str(name, LABEL_WIDTH, "…");
    let pad = LABEL_WIDTH.saturating_sub(console::measure_text_width(&fitted));
    format!("{fitted}{}", " ".repeat(pad))
}

/// How long the name rests at the start of each loop.
const MARQUEE_PAUSE_MS: u128 = 1000;
/// Time per one-column step of the scroll.
const MARQUEE_STEP_MS: u128 = 150;
const MARQUEE_GAP: &str = " · ";

/// One column of the scrolling loop; a wide glyph is `Wide` then `Cont`.
#[derive(Clone, Copy)]
enum Cell {
    Narrow(char),
    Wide(char),
    Cont,
}

/// Exactly `width` display columns of the name: fixed if it fits, else a
/// window into the looping `"{name} · "` that scrolls with `elapsed`.
/// A window edge that would cut a wide glyph shows a space instead.
fn marquee_window(name: &str, width: usize, elapsed: Duration) -> String {
    if console::measure_text_width(name) <= width {
        return download_label(name);
    }
    let mut cells = Vec::new();
    for c in name.chars().chain(MARQUEE_GAP.chars()) {
        match console::measure_text_width(c.encode_utf8(&mut [0; 4])) {
            0 => {}
            1 => cells.push(Cell::Narrow(c)),
            _ => cells.extend([Cell::Wide(c), Cell::Cont]),
        }
    }
    let cycle = MARQUEE_PAUSE_MS + cells.len() as u128 * MARQUEE_STEP_MS;
    let t = elapsed.as_millis() % cycle;
    let offset = t.saturating_sub(MARQUEE_PAUSE_MS) / MARQUEE_STEP_MS;
    let offset = offset as usize;
    let mut out = String::new();
    let mut i = 0;
    while i < width {
        match cells.get((offset + i) % cells.len()) {
            Some(Cell::Narrow(c)) => {
                out.push(*c);
                i += 1;
            }
            Some(Cell::Wide(c)) if i + 2 <= width => {
                out.push(*c);
                i += 2;
            }
            _ => {
                out.push(' ');
                i += 1;
            }
        }
    }
    out
}

/// Byte count with one decimal in every unit, right-aligned in `SIZE_WIDTH`.
fn size_field(bytes: u64) -> String {
    let text = if bytes < 1024 {
        format!("{bytes}.0 B")
    } else {
        format_size(bytes)
    };
    format!("{text:>SIZE_WIDTH$}")
}

fn percent_field(done: u64, total: u64) -> String {
    let percent = if total == 0 {
        0
    } else {
        (u128::from(done) * 100 / u128::from(total)).min(100)
    };
    format!("{percent:>3}")
}

fn speed_field(bytes_per_sec: f64) -> String {
    // Float-to-int casts saturate, and NaN becomes 0.
    size_field(bytes_per_sec as u64)
}

/// `mm:ss`, or `--:--` when unknown or past 99:59.
fn eta_field(seconds: Option<u64>) -> String {
    match seconds {
        Some(seconds) if seconds < 100 * 60 => format!("{:02}:{:02}", seconds / 60, seconds % 60),
        _ => "--:--".to_string(),
    }
}

/// Seconds left from the bar's own estimate; unknown until bytes flow.
fn eta_seconds(state: &ProgressState) -> Option<u64> {
    if state.pos() == 0 || state.per_sec() <= 0.0 {
        None
    } else {
        Some(state.eta().as_secs())
    }
}

/// The pacman-style style once the total size is known; every field but
/// the bar is rendered by custom keys so widths never change.
fn bar_style(colored: bool, bar: usize, fields: Fields, name: &str) -> ProgressStyle {
    let (spinner, bar) = if colored {
        ("{spinner:.cyan}".to_string(), format!("{{bar:{bar}.cyan}}"))
    } else {
        ("{spinner}".to_string(), format!("{{bar:{bar}}}"))
    };
    let tail = match fields {
        Fields::Full => "  {speed}/s  {eta}",
        Fields::NoEta => "  {speed}/s",
        Fields::Minimal => "",
    };
    let template = format!("  {spinner} {{label}}  {{done}} / {{total}}  [{bar}]  {{pct}}%{tail}");
    keyed(styled(&template), name)
}

/// Style used when the server did not announce a size.
fn unsized_style(colored: bool, name: &str) -> ProgressStyle {
    let spinner = if colored {
        "{spinner:.cyan}"
    } else {
        "{spinner}"
    };
    keyed(
        styled(&format!("  {spinner} {{label}}  {{done}}  {{speed}}/s")),
        name,
    )
}

fn keyed(style: ProgressStyle, name: &str) -> ProgressStyle {
    let name = name.to_string();
    style
        .with_key("label", move |state: &ProgressState, w: &mut dyn Write| {
            let _ = w.write_str(&marquee_window(&name, LABEL_WIDTH, state.elapsed()));
        })
        .with_key("done", |state: &ProgressState, w: &mut dyn Write| {
            let _ = w.write_str(&size_field(state.pos()));
        })
        .with_key("total", |state: &ProgressState, w: &mut dyn Write| {
            let _ = w.write_str(&size_field(state.len().unwrap_or(0)));
        })
        .with_key("pct", |state: &ProgressState, w: &mut dyn Write| {
            let _ = w.write_str(&percent_field(state.pos(), state.len().unwrap_or(0)));
        })
        .with_key("speed", |state: &ProgressState, w: &mut dyn Write| {
            let _ = w.write_str(&speed_field(state.per_sec()));
        })
        .with_key("eta", |state: &ProgressState, w: &mut dyn Write| {
            let _ = w.write_str(&eta_field(eta_seconds(state)));
        })
}

/// Drives the step's own bar while bytes stream.
///
/// Holds a clone of the step's bar, so the step still clears it before the
/// finished line, or on failure. Draws nothing when the step is not animated.
pub struct DownloadBar {
    bar: Option<ProgressBar>,
    name: String,
    colored: bool,
}

impl DownloadProgress for DownloadBar {
    fn start(&self, total: Option<u64>) {
        let Some(bar) = &self.bar else {
            return;
        };
        let style = match total {
            Some(_) => {
                let (width, fields) = bar_width(terminal_width());
                bar_style(self.colored, width, fields, &self.name)
            }
            None => unsized_style(self.colored, &self.name),
        };
        bar.set_style(style);
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
            name: label.to_string(),
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

pub fn plain_line(finished: &str) -> String {
    format!("  ✔ {finished}")
}

#[cfg(test)]
mod tests;
