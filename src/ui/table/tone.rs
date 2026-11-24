//! Semantic tones, SGR serialisation and cell styling.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::Mode;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tone {
    Plain,
    Dim,
    Bold,
    Green,
    GreenBold,
    Yellow,
    Cyan,
    CyanBold,
    Red,
    RedBold,
    Magenta,
}

impl Tone {
    pub(super) fn style(self) -> Style {
        match self {
            Tone::Plain => Style::default(),
            Tone::Dim => Style::default().add_modifier(Modifier::DIM),
            Tone::Bold => Style::default().add_modifier(Modifier::BOLD),
            Tone::Green => Style::default().fg(Color::Green),
            Tone::GreenBold => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Tone::Yellow => Style::default().fg(Color::Yellow),
            Tone::Cyan => Style::default().fg(Color::Cyan),
            Tone::CyanBold => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Tone::Red => Style::default().fg(Color::Red),
            Tone::RedBold => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            Tone::Magenta => Style::default().fg(Color::Magenta),
        }
    }
}

pub(super) fn sgr(fg: Color, modifier: Modifier) -> String {
    let mut codes = Vec::new();
    if modifier.contains(Modifier::BOLD) {
        codes.push("1");
    }
    if modifier.contains(Modifier::DIM) {
        codes.push("2");
    }
    match fg {
        Color::Red => codes.push("31"),
        Color::Green => codes.push("32"),
        Color::Yellow => codes.push("33"),
        Color::Magenta => codes.push("35"),
        Color::Cyan => codes.push("36"),
        _ => {}
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

pub(super) const RESET: &str = "\x1b[0m";

pub(super) fn paint(mode: Mode, tone: Tone, text: &str) -> String {
    let style = tone.style();
    let prefix = sgr(style.fg.unwrap_or(Color::Reset), style.add_modifier);
    if mode.colored && !prefix.is_empty() && !text.is_empty() {
        format!("{prefix}{text}{RESET}")
    } else {
        text.to_string()
    }
}

pub fn bold(mode: Mode, text: &str) -> String {
    paint(mode, Tone::Bold, text)
}

/// Semantic colour for a body cell, from its column heading and text.
pub(super) fn tone_for(label: &str, value: &str) -> Tone {
    let lower = value.to_ascii_lowercase();
    match label {
        "Action" => {
            if lower.starts_with("install (upstream)") {
                Tone::Magenta
            } else if lower.starts_with("relocate") {
                Tone::Yellow
            } else if lower.starts_with("install") {
                Tone::Green
            } else if lower.starts_with("upgrade") {
                Tone::Cyan
            } else if lower.starts_with("remove") {
                Tone::Red
            } else {
                Tone::Plain
            }
        }
        "Source" => match lower.as_str() {
            "upstream" => Tone::Magenta,
            _ => Tone::Plain,
        },
        "Bottle" => match lower.as_str() {
            "upstream release" => Tone::Magenta,
            "relocation needed" => Tone::Yellow,
            _ => Tone::Plain,
        },
        "Assessment" => match lower.as_str() {
            "blocked" => Tone::RedBold,
            "unresolved" => Tone::Red,
            "candidate" => Tone::Green,
            _ => Tone::Plain,
        },
        "Type" => match lower.as_str() {
            "cask" => Tone::Magenta,
            "formula" => Tone::Cyan,
            _ => Tone::Plain,
        },
        "Status" => match lower.as_str() {
            "pinned" => Tone::Yellow,
            "ready" => Tone::Green,
            _ => Tone::Plain,
        },
        _ => Tone::Plain,
    }
}

pub(super) const VERSION_ARROW: &str = " → ";

pub(super) fn style_line(label: &str, text: &str) -> Line<'static> {
    if let Some((old, new)) = text.split_once(VERSION_ARROW) {
        return Line::from(vec![
            Span::styled(old.to_string(), Tone::Dim.style()),
            Span::styled(VERSION_ARROW.to_string(), Tone::Dim.style()),
            Span::styled(
                new.to_string(),
                Tone::Green.style().add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    Line::from(Span::styled(
        text.to_string(),
        tone_for(label, text).style(),
    ))
}
