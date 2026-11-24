//! Column-width fitting for boxed panels.

use console::measure_text_width as cells;
use ratatui::text::Line;

use super::Panel;
use super::format::{FIELD_GAP, clean_cell};
use super::tone::style_line;

pub(super) fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
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

pub(super) enum PRow {
    Label(String),
    Data(Vec<Vec<Line<'static>>>),
}

pub(super) struct Layout {
    pub(super) title: String,
    pub(super) headers: Vec<String>,
    pub(super) widths: Vec<usize>,
    pub(super) indent: usize,
    pub(super) body: Vec<PRow>,
    pub(super) summary: Vec<(String, Vec<String>)>,
    pub(super) summary_label: usize,
    pub(super) width: u16,
    pub(super) height: u16,
}

impl<'a> Panel<'a> {
    /// Fit column widths to `width`; `None` when no panel can fit.
    pub(super) fn layout(&self, width: Option<u16>) -> Option<Layout> {
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
}
