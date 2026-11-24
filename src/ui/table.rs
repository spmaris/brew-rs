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
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    const BOXED: Mode = Mode {
        colored: false,
        boxed: true,
    };
    const STYLED: Mode = Mode {
        colored: true,
        boxed: true,
    };

    fn strip_ansi(text: &str) -> String {
        console::strip_ansi_codes(text).into_owned()
    }

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(ToString::to_string).collect()
    }

    fn plan_row(action: &'static str, note: &str, cells: &[&str]) -> Row {
        Row::plan(action, note.to_string(), row(cells))
    }

    fn plan_panel() -> Panel<'static> {
        Panel {
            title: "Install plan",
            headers: &["Package", "Version", "Source"],
            sections: vec![
                Section::new(
                    "Installing",
                    vec![plan_row("install", "", &["ripgrep", "14.1.1", "homebrew"])],
                ),
                Section::new("Promoting", Vec::new()),
                Section::new(
                    "Installing dependencies",
                    vec![plan_row("install", "", &["pcre2", "10.44", "homebrew"]).requested(false)],
                ),
            ],
            summary: Vec::new(),
        }
    }

    fn summary_rows() -> [(&'static str, String); 3] {
        [
            ("Install", "2 packages (1 requested, 1 dependency)".into()),
            ("Target", "arm64_sequoia".into()),
            ("Prefix", "~/p".into()),
        ]
    }

    #[test]
    fn mode_decision_covers_every_input() {
        let base = ModeInputs {
            is_tty: true,
            term_dumb: false,
            no_color: false,
            config_colored: true,
        };
        let decide = |change: fn(&mut ModeInputs)| {
            let mut inputs = base;
            change(&mut inputs);
            Mode::decide(inputs)
        };
        assert_eq!(Mode::decide(base), STYLED);
        assert_eq!(decide(|i| i.is_tty = false), Mode::PLAIN);
        assert_eq!(decide(|i| i.term_dumb = true), Mode::PLAIN);
        assert_eq!(decide(|i| i.no_color = true), BOXED);
        assert_eq!(decide(|i| i.config_colored = false), BOXED);
        // Redirected output is plain even when colour is requested.
        assert_eq!(
            decide(|i| {
                i.is_tty = false;
                i.config_colored = true;
            }),
            Mode::PLAIN
        );
    }

    #[test]
    fn plain_flat_table_is_space_aligned_with_header() {
        let panel = Panel::flat(
            "Installed (2)",
            &["Package", "Version", "Source", "Reason"],
            &[
                row(&["猫", "1\n2", "homebrew", "requested"]),
                row(&["ripgrep", "14.1.1", "upstream", "dependency"]),
            ],
        );
        assert_eq!(
            panel.render(Mode::PLAIN, Some(10)),
            "Package  Version  Source    Reason\n\
             猫       1 2      homebrew  requested\n\
             ripgrep  14.1.1   upstream  dependency"
        );
    }

    #[test]
    fn plain_plan_table_leads_with_action_and_ends_with_reason_and_note() {
        assert_eq!(
            plan_panel().render(Mode::PLAIN, None),
            "Action   Package  Version  Source    Reason      Note\n\
             install  ripgrep  14.1.1   homebrew  requested   -\n\
             install  pcre2    10.44    homebrew  dependency  -"
        );
    }

    #[test]
    fn plain_plan_replaces_spaces_in_a_note_with_dashes() {
        let panel = Panel {
            title: "Install plan",
            headers: &["Package", "Version", "Source"],
            sections: vec![Section::new(
                "Installing",
                vec![plan_row(
                    "install",
                    "needs a long note",
                    &["hello", "1.2", "homebrew"],
                )],
            )],
            summary: Vec::new(),
        };
        let rendered = panel.render(Mode::PLAIN, None);
        assert!(rendered.contains("needs-a-long-note"), "{rendered}");
        assert!(!rendered.contains("needs a long note"), "{rendered}");
    }

    #[test]
    fn plain_plan_uses_single_action_tokens_and_prints_dash_for_an_empty_note() {
        let panel = Panel {
            title: "Install plan",
            headers: &["Package", "Version", "Source"],
            sections: vec![Section::new(
                "Already installed",
                vec![plan_row(
                    "already-installed",
                    "",
                    &["hello", "1.2", "homebrew"],
                )],
            )],
            summary: Vec::new(),
        };
        let rendered = panel.render(Mode::PLAIN, None);
        assert_eq!(
            rendered,
            "Action             Package  Version  Source    Reason     Note\n\
             already-installed  hello    1.2      homebrew  requested  -"
        );
    }

    #[test]
    fn boxed_plan_without_exceptions_has_neither_action_nor_note_column() {
        let text = plan_panel().render(BOXED, None);
        assert!(!text.contains("Action"), "{text}");
        assert!(!text.contains("Note"), "{text}");
        assert!(text.contains("Package") && text.contains("Version") && text.contains("Source"));
    }

    #[test]
    fn boxed_plan_shows_a_note_column_for_a_keg_only_row() {
        let panel = Panel {
            title: "Install plan",
            headers: &["Package", "Version", "Source"],
            sections: vec![Section::new(
                "Installing",
                vec![
                    plan_row("install", "", &["ripgrep", "14.1.1", "homebrew"]),
                    plan_row("install", "keg-only", &["openssl@3", "3.0", "homebrew"]),
                ],
            )],
            summary: Vec::new(),
        };
        let text = panel.render(BOXED, None);
        assert!(text.contains("Note"), "{text}");
        assert!(text.contains("keg-only"), "{text}");
        let openssl = text
            .lines()
            .find(|line| line.contains("openssl@3"))
            .expect("openssl row");
        assert!(openssl.contains("keg-only"), "{openssl:?}");
    }

    #[test]
    fn boxed_panel_has_rounded_corners_title_header_and_sections() {
        let text = plan_panel().render(BOXED, None);
        assert!(!text.contains('\x1b'));
        assert_eq!(
            text,
            "╭─ Install plan ───────────────╮\n\
             │ Package    Version  Source   │\n\
             │ ──────────────────────────── │\n\
             │ Installing                   │\n\
             │   ripgrep  14.1.1   homebrew │\n\
             │ Installing dependencies      │\n\
             │   pcre2    10.44    homebrew │\n\
             ╰──────────────────────────────╯"
        );
    }

    fn assert_full_width(text: &str, width: usize) {
        assert!(
            text.lines().all(|line| cells(line) == width),
            "not {width} wide:\n{text}"
        );
    }

    #[test]
    fn panels_fill_the_width_and_keep_columns_compact() {
        let narrow = plan_panel().render(BOXED, Some(80));
        assert_full_width(&narrow, 80);
        let wide = plan_panel().render(BOXED, Some(120));
        assert_full_width(&wide, 120);
        // Spare width goes to the last column, so every earlier column keeps
        // its natural width and starts at the same place; Source, being last,
        // only grows to the right.
        let header = |text: &str| text.lines().nth(1).unwrap().to_string();
        let start = |line: &str, word: &str| cells(&line[..line.find(word).unwrap()]);
        let (natural, narrow_header, wide_header) = (
            header(&plan_panel().render(BOXED, None)),
            header(&narrow),
            header(&wide),
        );
        for word in ["Version", "Source"] {
            assert_eq!(start(&narrow_header, word), start(&natural, word), "{word}");
            assert_eq!(start(&wide_header, word), start(&natural, word), "{word}");
        }
    }

    #[test]
    fn panel_width_fills_the_terminal_and_defaults_to_100() {
        assert_eq!(panel_width(Some(80)), 80);
        assert_eq!(panel_width(Some(120)), 120);
        assert_eq!(panel_width(Some(200)), 200);
        assert_eq!(panel_width(None), 100);
        let full = plan_panel().render(BOXED, Some(panel_width(Some(200))));
        assert_full_width(&full, 200);
    }

    #[test]
    fn summary_rows_sit_inside_the_borders() {
        let mut panel = plan_panel();
        panel.summary = summary_rows()
            .iter()
            .map(|(label, value)| (label.to_string(), value.clone()))
            .collect();
        let text = panel.render(BOXED, Some(60));
        assert_eq!(
            text,
            "╭─ Install plan ───────────────────────────────────────────╮\n\
             │ Package    Version  Source                               │\n\
             │ ──────────────────────────────────────────────────────── │\n\
             │ Installing                                               │\n\
             │   ripgrep  14.1.1   homebrew                             │\n\
             │ Installing dependencies                                  │\n\
             │   pcre2    10.44    homebrew                             │\n\
             │ ──────────────────────────────────────────────────────── │\n\
             │ Summary                                                  │\n\
             │   Install    2 packages (1 requested, 1 dependency)      │\n\
             │   Target     arm64_sequoia                               │\n\
             │   Prefix     ~/p                                         │\n\
             ╰──────────────────────────────────────────────────────────╯"
        );
        let styled = panel.render(STYLED, Some(60));
        assert_eq!(strip_ansi(&styled), text);
        assert!(!styled.contains("\x1b[2mPrefix"), "{styled:?}");
        assert!(styled.contains("\x1b[1;36mSummary\x1b[0m"), "{styled:?}");
    }

    #[test]
    fn long_summary_values_wrap_inside_the_panel() {
        let mut panel = plan_panel();
        panel.summary = vec![(
            "Prefix".into(),
            "~/Library/Application Support/brew-rs/native/with/a/very/long/path".into(),
        )];
        let text = panel.render(BOXED, Some(50));
        assert_full_width(&text, 50);
        assert!(text.contains("Prefix"), "{text}");
    }

    #[test]
    fn plain_render_excludes_the_summary_which_is_rendered_separately() {
        let summary = summary_rows();
        let panel =
            Panel::flat("T", &["Package", "Version"], &[row(&["a", "1"])]).with_summary(&summary);
        let plain = panel.render(Mode::PLAIN, None);
        assert_eq!(plain, "Package  Version\na        1");
        assert!(!plain.contains("Summary"));
        assert!(!plain.contains('│'));
        assert_eq!(
            panel.plain_summary(Mode::PLAIN),
            "Summary\n\
             \x20 Install    2 packages (1 requested, 1 dependency)\n\
             \x20 Target     arm64_sequoia\n\
             \x20 Prefix     ~/p"
        );
        let stacked = Panel::flat(
            "T",
            &["Package", "Available"],
            &[row(&["a-very-long-name", "2"])],
        )
        .with_summary(&summary)
        .render(BOXED, Some(12));
        assert!(stacked.ends_with("  Prefix     ~/p"), "{stacked}");
    }

    #[test]
    fn plain_upgrade_table_splits_versions_and_adds_reason_and_note() {
        let panel = Panel {
            title: "Upgrade plan",
            headers: &["Package", "Version", "Source"],
            sections: vec![Section::new(
                "Upgrading",
                vec![plan_row("upgrade", "", &["hello", "1.2 → 1.3", "homebrew"])],
            )],
            summary: Vec::new(),
        };
        assert_eq!(
            panel.render(Mode::PLAIN, None),
            "Action   Package  From  To   Source    Reason     Note\n\
             upgrade  hello    1.2   1.3  homebrew  requested  -"
        );
    }

    #[test]
    fn styled_panel_colours_semantically_and_strips_to_the_boxed_text() {
        let styled = plan_panel().render(STYLED, Some(80));
        assert_eq!(strip_ansi(&styled), plan_panel().render(BOXED, Some(80)));
        assert_eq!(strip_ansi(&styled).lines().next().map(cells), Some(80));
        assert!(styled.contains("\x1b[1;36mInstalling\x1b[0m"));
        assert!(styled.contains("\x1b[1mPackage\x1b[0m"));
        assert!(!styled.contains("\x1b[2mpcre2"), "{styled:?}");
    }

    #[test]
    fn version_change_dims_old_and_arrow_and_bolds_new() {
        let panel = Panel::flat(
            "Upgrade plan",
            &["Package", "Version", "Source"],
            &[row(&["hello", "1.2 → 1.3", "homebrew"])],
        );
        let styled = panel.render(STYLED, Some(80));
        assert!(
            styled.contains("\x1b[2m1.2 → \x1b[0m\x1b[1;32m1.3\x1b[0m"),
            "{styled:?}"
        );
    }

    #[test]
    fn dependency_rows_are_not_dimmed() {
        let styled = plan_panel().render(STYLED, Some(80));
        let dependency = styled
            .lines()
            .find(|line| line.contains("pcre2"))
            .expect("dependency row");
        assert!(!dependency.contains("\x1b[2m"), "{dependency:?}");
    }

    #[test]
    fn divider_rules_stay_dim() {
        let styled = plan_panel().render(STYLED, Some(80));
        assert!(styled.contains("\x1b[2m─"), "{styled:?}");
    }

    #[test]
    fn long_cells_wrap_to_the_terminal_width() {
        let path = "/Users/example/Library/Application Support/brew-rs/native/Cellar/pkg/1.0";
        let panel = Panel::flat(
            "Paths",
            &["Package", "Path"],
            &[row(&["pkg", path]), row(&["other", "short"])],
        );
        let text = panel.render(BOXED, Some(40));
        assert!(text.lines().all(|line| cells(line) <= 40), "{text}");
        assert!(text.lines().count() > 6, "{text}");
        assert!(text.contains("other"));
        let flattened: String = text
            .lines()
            .filter_map(|line| line.strip_prefix("│ pkg "))
            .chain(text.lines().filter(|line| line.starts_with("│       ")))
            .map(|line| line.trim_matches(|c| c == '│' || c == ' '))
            .collect();
        assert!(flattened.contains("/Users/example/Library"), "{text}");
    }

    #[test]
    fn too_narrow_for_a_panel_stacks_fields() {
        let panel = Panel::flat(
            "Packages",
            &["Package", "Available"],
            &[row(&["example/tap/very-long-package-name", "2.0"])],
        );
        assert_eq!(
            panel.render(BOXED, Some(24)),
            "example/tap/very-long-package-name\n  Available       2.0"
        );
    }

    #[test]
    fn panel_draws_into_a_ratatui_test_backend() {
        let layout = plan_panel().layout(Some(80)).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(layout.width, layout.height)).unwrap();
        terminal
            .draw(|frame| layout.paint(frame.buffer_mut()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (right, bottom) = (layout.width - 1, layout.height - 1);
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(right, 0)].symbol(), "╮");
        assert_eq!(buffer[(0, bottom)].symbol(), "╰");
        assert_eq!(buffer[(right, bottom)].symbol(), "╯");
        let header = buffer[(2, 1)].clone();
        assert_eq!(header.symbol(), "P");
        assert!(header.modifier.contains(Modifier::BOLD));
        let first = buffer[(4, 4)].clone();
        assert_eq!(first.symbol(), "r");
        assert_eq!(first.fg, Color::Reset);
    }

    #[test]
    fn errors_are_red_bold_with_indented_hints() {
        let message =
            "No Homebrew formula named 'x'.\n  Did you mean: y?\nthird\n    brewrs install z";
        assert_eq!(
            format_error(Mode::PLAIN, message),
            "Error: No Homebrew formula named 'x'.\n  Did you mean: y?\n  third\n    brewrs install z"
        );
        assert_eq!(
            format_error(STYLED, message),
            "\x1b[1;31mError:\x1b[0m No Homebrew formula named 'x'.\n  Did you mean: y?\n  third\n    \x1b[1mbrewrs install z\x1b[0m"
        );
    }

    #[test]
    fn runnable_hints_are_bold_from_the_command_onwards() {
        let message = "Something failed.\n  Try: brewrs install neovim --verbose";
        let styled = format_error(STYLED, message);
        assert!(
            styled.contains("\n  Try: \x1b[1mbrewrs install neovim --verbose\x1b[0m"),
            "{styled:?}"
        );
        assert_eq!(
            format_error(BOXED, message),
            "Error: Something failed.\n  Try: brewrs install neovim --verbose"
        );
    }

    #[test]
    fn summary_block_aligns_plain_labels_under_a_bold_cyan_heading() {
        let rows = [
            ("Install", "2 packages".to_string()),
            ("Prefix", "~/p".into()),
        ];
        assert_eq!(
            summary_block(Mode::PLAIN, &rows),
            "Summary\n  Install    2 packages\n  Prefix     ~/p"
        );
        assert_eq!(
            summary_block(STYLED, &rows),
            "\x1b[1;36mSummary\x1b[0m\n  Install    2 packages\n  Prefix     ~/p"
        );
    }

    #[test]
    fn fields_are_indented_with_plain_labels_and_no_colon() {
        let rows = [("Bin", "~/bin".to_string()), ("Opt", "~/opt".into())];
        assert_eq!(fields(Mode::PLAIN, &rows), "  Bin    ~/bin\n  Opt    ~/opt");
        assert_eq!(fields(STYLED, &rows), "  Bin    ~/bin\n  Opt    ~/opt");
        assert_eq!(
            field(Mode::PLAIN, "Scope", "runtime\nsecond"),
            "  Scope           runtime\n                  second"
        );
    }

    #[test]
    fn result_lines_always_carry_the_check_and_colour_it_only_with_colour() {
        assert_eq!(
            result(STYLED, "Installed 1 formula"),
            "\x1b[1;32m✔ \x1b[0mInstalled 1 formula"
        );
        assert_eq!(
            result(BOXED, "Installed 2 formulae"),
            "✔ Installed 2 formulae"
        );
        assert_eq!(
            result(Mode::PLAIN, "Removed 1 formula"),
            "✔ Removed 1 formula"
        );
    }

    #[test]
    fn display_path_abbreviates_the_home_directory() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(display_path(&home.join("x/y")), "~/x/y");
        assert_eq!(display_path(&home), "~");
        assert_eq!(display_path(std::path::Path::new("/opt/x")), "/opt/x");
    }
}
