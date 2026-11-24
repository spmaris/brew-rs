//! The single table renderer used by every command.
//!
//! Output has two independent axes, decided once by [`Mode::decide`]: colour
//! (ANSI styling) and boxes (a rounded panel fitted to the terminal width).
//! Redirected output is plain space-aligned columns that scripts and `grep`
//! can rely on.
//!
//! Panels are laid out into a ratatui [`Buffer`] and serialised to a string,
//! so output lands in normal scrollback and the terminal is never queried or
//! switched into raw mode.

use console::measure_text_width as cells;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Padding, Widget},
};

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tone {
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
    fn style(self) -> Style {
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

fn sgr(fg: Color, modifier: Modifier) -> String {
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

const RESET: &str = "\x1b[0m";

fn paint(mode: Mode, tone: Tone, text: &str) -> String {
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
fn aligned_fields(rows: &[(&str, String)], min_width: usize) -> String {
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
const FIELD_GAP: usize = 4;

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

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<String>,
    /// Whether the row was asked for rather than pulled in as a dependency.
    pub requested: bool,
    /// One-token plan action (`install`, `upgrade`, `remove`, ...); empty
    /// outside plans.
    pub action: &'static str,
    /// Exceptional plan note (`keg-only`); empty otherwise.
    pub note: String,
}

impl Row {
    /// A plan row: its action token and note are rendered as dedicated
    /// columns, never parsed out of the cell text.
    pub fn plan(action: &'static str, note: String, cells: Vec<String>) -> Self {
        Self {
            cells,
            requested: true,
            action,
            note,
        }
    }

    /// Mark whether the row was requested directly or pulled in as a
    /// dependency; plain output derives its `Reason` column from this.
    pub fn requested(mut self, requested: bool) -> Self {
        self.requested = requested;
        self
    }
}

/// Rows under an optional dnf-style label (`Installing`, `Removing`, ...).
#[derive(Clone, Debug)]
pub struct Section {
    pub label: String,
    pub rows: Vec<Row>,
}

impl Section {
    pub fn new(label: &str, rows: Vec<Row>) -> Self {
        Self {
            label: label.into(),
            rows,
        }
    }
}

/// Semantic colour for a body cell, from its column heading and text.
fn tone_for(label: &str, value: &str) -> Tone {
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

const VERSION_ARROW: &str = " → ";

fn style_line(label: &str, text: &str) -> Line<'static> {
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

fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
    if cells(cell) <= width {
        return vec![cell.to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in cell.split_whitespace() {
        if !line.is_empty() {
            if cells(&line) + 1 + cells(word) <= width {
                line.push(' ');
            } else {
                lines.push(std::mem::take(&mut line));
            }
        }
        if cells(word) <= width {
            line.push_str(word);
        } else {
            for ch in word.chars() {
                let previous_len = line.len();
                line.push(ch);
                if previous_len != 0 && cells(&line) > width {
                    line.truncate(previous_len);
                    lines.push(std::mem::take(&mut line));
                    line.push(ch);
                }
            }
        }
    }
    lines.push(line);
    lines
}

/// A titled table. Flat tables have one unlabelled section.
pub struct Panel<'a> {
    pub title: &'a str,
    pub headers: &'a [&'a str],
    pub sections: Vec<Section>,
    /// `Label`/value rows drawn under a `Summary` heading at the bottom.
    pub summary: Vec<(String, String)>,
}

impl<'a> Panel<'a> {
    pub fn flat(title: &'a str, headers: &'a [&'a str], rows: &[Vec<String>]) -> Self {
        let reason = headers.iter().position(|label| *label == "Reason");
        let rows = rows
            .iter()
            .map(|cells| {
                let dependency = reason.is_some_and(|index| {
                    cells
                        .get(index)
                        .is_some_and(|cell| cell.eq_ignore_ascii_case("dependency"))
                });
                Row {
                    cells: cells.clone(),
                    requested: !dependency,
                    action: "",
                    note: String::new(),
                }
            })
            .collect();
        Self {
            title,
            headers,
            sections: vec![Section::new("", rows)],
            summary: Vec::new(),
        }
    }

    pub fn with_summary(mut self, summary: &[(&str, String)]) -> Self {
        self.summary = summary
            .iter()
            .map(|(label, value)| (clean_cell(label), clean_cell(value)))
            .collect();
        self
    }

    fn summary_rows(&self) -> Vec<(&str, String)> {
        self.summary
            .iter()
            .map(|(label, value)| (label.as_str(), value.clone()))
            .collect()
    }

    fn labelled(&self) -> bool {
        self.sections
            .iter()
            .any(|section| !section.label.is_empty())
    }

    fn cell(&self, row: &Row, index: usize) -> String {
        if index < self.headers.len() {
            clean_cell(row.cells.get(index).map_or("", String::as_str))
        } else {
            clean_cell(&row.note)
        }
    }

    /// Whether any row carries an exceptional note, so boxed panels only grow
    /// a Note column when there is something to show.
    fn has_note(&self) -> bool {
        self.sections
            .iter()
            .flat_map(|section| &section.rows)
            .any(|row| !row.note.is_empty())
    }

    /// Boxed headers: the base columns plus a `Note` column when needed.
    fn boxed_headers(&self) -> Vec<String> {
        let mut headers: Vec<String> = self.headers.iter().map(|label| clean_cell(label)).collect();
        if self.has_note() {
            headers.push("Note".into());
        }
        headers
    }

    pub fn render(&self, mode: Mode, width: Option<u16>) -> String {
        if self.headers.is_empty() {
            return String::new();
        }
        if !mode.boxed {
            // Plain output keeps the summary out of the table so callers can
            // print it to stderr and leave stdout machine-readable.
            return self.render_plain();
        }
        match self.layout(width) {
            Some(layout) => {
                let area = Rect::new(0, 0, layout.width, layout.height);
                let mut buffer = Buffer::empty(area);
                layout.paint(&mut buffer);
                buffer_to_string(&buffer, mode.colored)
            }
            None => self.with_summary_text(mode, self.render_stacked(mode)),
        }
    }

    /// The summary block as plain text, for callers that print it separately
    /// (to stderr) so redirected stdout carries only the table.
    pub fn plain_summary(&self, mode: Mode) -> String {
        if self.summary.is_empty() {
            String::new()
        } else {
            summary_block(mode, &self.summary_rows())
        }
    }

    /// Stacked output has no borders, so the summary follows the table as its
    /// own block.
    fn with_summary_text(&self, mode: Mode, table: String) -> String {
        if self.summary.is_empty() {
            table
        } else {
            format!("{table}\n\n{}", summary_block(mode, &self.summary_rows()))
        }
    }

    /// Space-aligned columns with a header row. Labelled (plan) tables lead
    /// with the Action token, drop the section rows so every line is
    /// self-describing for `grep`, and end with `Reason` and `Note` columns;
    /// an `old → new` Version cell splits into `From`/`To`.
    fn render_plain(&self) -> String {
        let labelled = self.labelled();
        let mut order: Vec<PlainColumn>;
        if labelled {
            order = vec![PlainColumn::Action];
            let version = self.headers.iter().position(|label| *label == "Version");
            let split = version.is_some_and(|version| {
                self.sections
                    .iter()
                    .flat_map(|section| &section.rows)
                    .any(|row| self.cell(row, version).contains(VERSION_ARROW))
            });
            for (index, _) in self.headers.iter().enumerate() {
                if split && Some(index) == version {
                    order.push(PlainColumn::From(index));
                    order.push(PlainColumn::To(index));
                } else {
                    order.push(PlainColumn::Cell(index));
                }
            }
            order.push(PlainColumn::Reason);
            order.push(PlainColumn::Note);
        } else {
            order = (0..self.headers.len()).map(PlainColumn::Cell).collect();
        }
        let mut table: Vec<Vec<String>> = vec![
            order
                .iter()
                .map(|column| match column {
                    PlainColumn::Cell(index) => clean_cell(self.headers[*index]),
                    PlainColumn::From(_) => "From".into(),
                    PlainColumn::To(_) => "To".into(),
                    PlainColumn::Action => "Action".into(),
                    PlainColumn::Reason => "Reason".into(),
                    PlainColumn::Note => "Note".into(),
                })
                .collect(),
        ];
        for section in &self.sections {
            for row in &section.rows {
                table.push(
                    order
                        .iter()
                        .map(|column| match column {
                            PlainColumn::Cell(index) => self.cell(row, *index),
                            PlainColumn::From(index) => {
                                let value = self.cell(row, *index);
                                match value.split_once(VERSION_ARROW) {
                                    Some((old, _)) => old.to_string(),
                                    None => value,
                                }
                            }
                            PlainColumn::To(index) => self
                                .cell(row, *index)
                                .split_once(VERSION_ARROW)
                                .map_or_else(String::new, |(_, new)| new.to_string()),
                            PlainColumn::Action => clean_cell(row.action),
                            PlainColumn::Reason => Self::reason_for(section, row).to_string(),
                            PlainColumn::Note => {
                                if row.note.is_empty() {
                                    "-".to_string()
                                } else {
                                    // Keep each note a single awk field.
                                    clean_cell(&row.note).replace(' ', "-")
                                }
                            }
                        })
                        .collect(),
                );
            }
        }
        let widths: Vec<usize> = (0..order.len())
            .map(|column| {
                table
                    .iter()
                    .map(|row| cells(&row[column]))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        table
            .iter()
            .map(|row| {
                let mut line = String::new();
                for (value, width) in row.iter().zip(&widths) {
                    line.push_str(value);
                    line.push_str(&" ".repeat(width - cells(value) + 2));
                }
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Too narrow for any panel: one block of fields per row.
    fn render_stacked(&self, mode: Mode) -> String {
        self.sections
            .iter()
            .flat_map(|section| &section.rows)
            .map(|row| {
                let mut lines = vec![bold(mode, &self.cell(row, 0))];
                lines.extend(
                    self.headers
                        .iter()
                        .enumerate()
                        .skip(1)
                        .map(|(index, label)| field(mode, label, &self.cell(row, index))),
                );
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Fit column widths to `width`; `None` when no panel can fit.
    fn layout(&self, width: Option<u16>) -> Option<Layout> {
        let headers = self.boxed_headers();
        let count = headers.len();
        let indent = if self.labelled() { 2 } else { 0 };
        let mut widths: Vec<usize> = (0..count)
            .map(|column| {
                let body = self
                    .sections
                    .iter()
                    .flat_map(|section| &section.rows)
                    .map(|row| {
                        cells(&self.cell(row, column)) + if column == 0 { indent } else { 0 }
                    })
                    .max()
                    .unwrap_or(0);
                body.max(cells(&headers[column]))
            })
            .collect();
        let label_width = self
            .sections
            .iter()
            .map(|section| cells(&section.label))
            .max()
            .unwrap_or(0);
        let title = clean_cell(self.title);
        let title_width = if title.is_empty() {
            0
        } else {
            cells(&title) + 4
        };
        // Border and padding take 4 columns.
        let chrome = 4 + 2 * (count - 1);
        let available = width.map(|width| usize::from(width).saturating_sub(chrome));
        if let Some(available) = available {
            let floors: Vec<usize> = (0..count)
                .map(|column| cells(&headers[column]).max(16).min(widths[column]))
                .collect();
            while widths.iter().sum::<usize>() > available {
                let over = widths.iter().sum::<usize>() - available;
                let shrinkable = (0..count)
                    .filter(|&column| (column > 0 || count == 1) && widths[column] > floors[column])
                    .max_by_key(|&column| widths[column])?;
                widths[shrinkable] -= over.min(widths[shrinkable] - floors[shrinkable]);
            }
            // Give all spare width to the last column.
            let spare = available - widths.iter().sum::<usize>();
            if let Some(last) = widths.last_mut() {
                *last += spare;
            }
        }
        let content = widths.iter().sum::<usize>() + 2 * (count - 1);
        let floor = content.max(label_width).max(title_width.saturating_sub(4));
        let summary_label = self
            .summary
            .iter()
            .map(|(label, _)| cells(label))
            .max()
            .unwrap_or(0);
        let value_offset = 2 + summary_label + FIELD_GAP;
        let inner = match width {
            Some(width) => floor.max(usize::from(width).saturating_sub(4)),
            None => self
                .summary
                .iter()
                .map(|(_, value)| value_offset + cells(value))
                .fold(floor, usize::max),
        };
        let total = u16::try_from(inner + 4).ok()?;

        let wrap = |column: usize, text: &str| {
            wrap_cell(
                text,
                widths[column].saturating_sub(if column == 0 { indent } else { 0 }),
            )
        };
        let mut body = Vec::new();
        for section in &self.sections {
            if section.rows.is_empty() {
                continue;
            }
            if !section.label.is_empty() {
                body.push(PRow::Label(clean_cell(&section.label)));
            }
            for row in &section.rows {
                body.push(PRow::Data(
                    (0..count)
                        .map(|column| {
                            wrap(column, &self.cell(row, column))
                                .into_iter()
                                .map(|line| style_line(&headers[column], &line))
                                .collect()
                        })
                        .collect(),
                ));
            }
        }
        let rows_height: usize = body
            .iter()
            .map(|row| match row {
                PRow::Label(_) => 1,
                PRow::Data(columns) => columns.iter().map(Vec::len).max().unwrap_or(1).max(1),
            })
            .sum();
        let value_width = inner.saturating_sub(value_offset).max(8);
        let summary: Vec<(String, Vec<String>)> = self
            .summary
            .iter()
            .map(|(label, value)| (label.clone(), wrap_cell(value, value_width)))
            .collect();
        let summary_height: usize = if summary.is_empty() {
            0
        } else {
            2 + summary.iter().map(|(_, lines)| lines.len()).sum::<usize>()
        };
        Some(Layout {
            title,
            headers,
            widths,
            indent,
            body,
            summary,
            summary_label,
            width: total,
            height: u16::try_from(rows_height + summary_height + 4).ok()?,
        })
    }

    /// Why a plan row is present: the dependency sections always mean a
    /// dependency; otherwise the row's own request flag decides.
    fn reason_for(section: &Section, row: &Row) -> &'static str {
        if matches!(
            section.label.as_str(),
            "Installing dependencies" | "Removing unused dependencies"
        ) || !row.requested
        {
            "dependency"
        } else {
            "requested"
        }
    }
}

/// A plain-output column: an existing header/cell index, one half of a split
/// `old → new` version, or a derived `Action`, `Reason` or `Note`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlainColumn {
    Cell(usize),
    From(usize),
    To(usize),
    Action,
    Reason,
    Note,
}

enum PRow {
    Label(String),
    Data(Vec<Vec<Line<'static>>>),
}

struct Layout {
    title: String,
    headers: Vec<String>,
    widths: Vec<usize>,
    indent: usize,
    body: Vec<PRow>,
    summary: Vec<(String, Vec<String>)>,
    summary_label: usize,
    width: u16,
    height: u16,
}

impl Layout {
    fn paint(&self, buffer: &mut Buffer) {
        let area = buffer.area;
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1));
        if !self.title.is_empty() {
            block = block.title(Line::from(Span::styled(
                format!("─ {} ", self.title),
                Tone::Bold.style(),
            )));
        }
        let inner = block.inner(area);
        block.render(area, buffer);

        let columns: Vec<u16> = self
            .widths
            .iter()
            .scan(0usize, |offset, width| {
                let x = *offset;
                *offset += width + 2;
                Some(u16::try_from(inner.x as usize + x).unwrap_or(u16::MAX))
            })
            .collect();
        let mut y = inner.y;
        for (index, label) in self.headers.iter().enumerate() {
            buffer.set_stringn(
                columns[index],
                y,
                label,
                self.widths[index],
                Tone::Bold.style(),
            );
        }
        y += 1;
        buffer.set_stringn(
            inner.x,
            y,
            "─".repeat(inner.width as usize),
            inner.width as usize,
            Tone::Dim.style(),
        );
        y += 1;
        for row in &self.body {
            match row {
                PRow::Label(label) => {
                    buffer.set_stringn(
                        inner.x,
                        y,
                        label,
                        inner.width as usize,
                        Tone::CyanBold.style(),
                    );
                    y += 1;
                }
                PRow::Data(columns_lines) => {
                    let height = columns_lines.iter().map(Vec::len).max().unwrap_or(1).max(1);
                    for (column, lines) in columns_lines.iter().enumerate() {
                        let pad = if column == 0 { self.indent } else { 0 };
                        let x = columns[column] + pad as u16;
                        let width = self.widths[column].saturating_sub(pad);
                        for (offset, line) in lines.iter().enumerate() {
                            buffer.set_line(x, y + offset as u16, line, width as u16);
                        }
                    }
                    y += height as u16;
                }
            }
        }
        if self.summary.is_empty() {
            return;
        }
        buffer.set_stringn(
            inner.x,
            y,
            "─".repeat(inner.width as usize),
            inner.width as usize,
            Tone::Dim.style(),
        );
        buffer.set_stringn(
            inner.x,
            y + 1,
            "Summary",
            inner.width as usize,
            Tone::CyanBold.style(),
        );
        y += 2;
        let value_x = inner.x + (2 + self.summary_label + FIELD_GAP) as u16;
        let value_width = inner.right().saturating_sub(value_x) as usize;
        for (label, lines) in &self.summary {
            buffer.set_stringn(
                inner.x + 2,
                y,
                label,
                self.summary_label,
                Tone::Plain.style(),
            );
            for line in lines {
                buffer.set_stringn(value_x, y, line, value_width, Tone::Plain.style());
                y += 1;
            }
        }
    }
}

fn buffer_to_string(buffer: &Buffer, colored: bool) -> String {
    let area = buffer.area;
    let plain = (Color::Reset, Modifier::empty());
    let mut lines = Vec::new();
    for y in area.top()..area.bottom() {
        let mut line = String::new();
        let mut active = plain;
        let mut skip = 0;
        for x in area.left()..area.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let cell = &buffer[(x, y)];
            let key = (cell.fg, cell.modifier);
            if colored && key != active {
                if active != plain {
                    line.push_str(RESET);
                }
                line.push_str(&sgr(key.0, key.1));
                active = key;
            }
            line.push_str(cell.symbol());
            skip = cells(cell.symbol()).saturating_sub(1);
        }
        if colored && active != plain {
            line.push_str(RESET);
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Panel width when the terminal width can't be detected.
const DEFAULT_PANEL_WIDTH: u16 = 100;

/// Terminal width, when known.
pub fn terminal_width() -> Option<u16> {
    let term = console::Term::stdout();
    term.is_term()
        .then(|| term.size_checked())
        .flatten()
        .map(|(_, columns)| columns)
        .filter(|columns| *columns > 0)
}

/// Columns every panel fills: the full terminal width, measured when the
/// panel is printed, or 100 when the width is unknown.
pub fn panel_width(terminal: Option<u16>) -> u16 {
    terminal.unwrap_or(DEFAULT_PANEL_WIDTH)
}

#[cfg(test)]
mod tests;
