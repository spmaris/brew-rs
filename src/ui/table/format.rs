//! Free-standing formatting: cells, paths, fields, summaries, results and errors.

use console::measure_text_width as cells;

use super::Mode;
use super::tone::{Tone, paint};

pub fn clean_cell(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// `~`-abbreviate a path under the home directory for display.
pub fn display_path(path: &std::path::Path) -> String {
    let full = path.display().to_string();
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => full,
        },
        None => full,
    }
}

/// Aligned, indented `Label    value` lines with labels padded to the widest
/// label (at least `min_width`); continuation lines stay under the value.
pub(super) fn aligned_fields(rows: &[(&str, String)], min_width: usize) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| cells(&clean_cell(label)))
        .max()
        .unwrap_or(0)
        .max(min_width);
    let indent = " ".repeat(2 + width + FIELD_GAP);
    rows.iter()
        .map(|(label, value)| {
            let label = clean_cell(label);
            let padding = " ".repeat(width - cells(&label) + FIELD_GAP);
            let mut lines = value.split('\n').map(clean_cell);
            let mut result = format!("  {}{padding}{}", label, lines.next().unwrap_or_default());
            for line in lines {
                result.push('\n');
                result.push_str(&indent);
                result.push_str(&line);
            }
            result
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Spaces between the widest label and its value.
pub(super) const FIELD_GAP: usize = 4;

/// One `  Label    value` line. Labels pad to a common minimum width so
/// separate calls line up with each other.
pub fn field(_mode: Mode, label: &str, value: &str) -> String {
    aligned_fields(&[(label, value.to_string())], 12)
}

/// A group of `  Label    value` lines aligned to each other.
pub fn fields(_mode: Mode, rows: &[(&str, String)]) -> String {
    aligned_fields(rows, 0)
}

/// A bold cyan `Summary` heading over aligned, labelled lines.
pub fn summary_block(mode: Mode, rows: &[(&str, String)]) -> String {
    format!(
        "{}\n{}",
        paint(mode, Tone::CyanBold, "Summary"),
        fields(mode, rows)
    )
}

/// Outcome line: a check mark and the text; the mark is green and bold only
/// when colour is on.
pub fn result(mode: Mode, text: &str) -> String {
    let text = clean_cell(text);
    let check = if mode.colored {
        paint(mode, Tone::GreenBold, "✔ ")
    } else {
        "✔ ".to_string()
    };
    format!("{check}{text}")
}

/// Prefix of a runnable command inside an error hint.
const RUNNABLE_COMMAND: &str = "brewrs ";

/// `Error:` in bold red, then the message; hint lines are indented and keep
/// any deeper indentation they carry. A hint that names a runnable command
/// shows the command in bold, from its name to the end of the line.
pub fn format_error(mode: Mode, message: &str) -> String {
    let mut lines = message.lines();
    let first = lines.next().unwrap_or_default();
    let mut out = format!("{} {first}", paint(mode, Tone::RedBold, "Error:"));
    for line in lines {
        out.push('\n');
        if !line.starts_with(' ') && !line.starts_with('\t') {
            out.push_str("  ");
        }
        match line.find(RUNNABLE_COMMAND).filter(|_| mode.colored) {
            Some(index) => {
                let (before, command) = line.split_at(index);
                out.push_str(before);
                out.push_str(&paint(mode, Tone::Bold, command));
            }
            None => out.push_str(line),
        }
    }
    out
}
