//! Choosing how a panel is rendered: plain, boxed or stacked.

use ratatui::{buffer::Buffer, layout::Rect};

use super::format::{field, summary_block};
use super::paint::buffer_to_string;
use super::tone::bold;
use super::{Mode, Panel};

impl<'a> Panel<'a> {
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
    pub(super) fn with_summary_text(&self, mode: Mode, table: String) -> String {
        if self.summary.is_empty() {
            table
        } else {
            format!("{table}\n\n{}", summary_block(mode, &self.summary_rows()))
        }
    }

    /// Too narrow for any panel: one block of fields per row.
    pub(super) fn render_stacked(&self, mode: Mode) -> String {
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
}
