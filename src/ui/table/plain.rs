//! Plain, space-aligned output for redirected stdout.

use console::measure_text_width as cells;

use super::Panel;
use super::format::clean_cell;
use super::tone::VERSION_ARROW;

/// A plain-output column: an existing header/cell index, one half of a split
/// `old → new` version, or a derived `Action`, `Reason` or `Note`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PlainColumn {
    Cell(usize),
    From(usize),
    To(usize),
    Action,
    Reason,
    Note,
}

impl<'a> Panel<'a> {
    /// Space-aligned columns with a header row. Labelled (plan) tables lead
    /// with the Action token, drop the section rows so every line is
    /// self-describing for `grep`, and end with `Reason` and `Note` columns;
    /// an `old → new` Version cell splits into `From`/`To`.
    pub(super) fn render_plain(&self) -> String {
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
}
