//! The table model (rows, sections, panels) and its builder.

use super::format::clean_cell;

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

    pub(super) fn summary_rows(&self) -> Vec<(&str, String)> {
        self.summary
            .iter()
            .map(|(label, value)| (label.as_str(), value.clone()))
            .collect()
    }

    pub(super) fn labelled(&self) -> bool {
        self.sections
            .iter()
            .any(|section| !section.label.is_empty())
    }

    pub(super) fn cell(&self, row: &Row, index: usize) -> String {
        if index < self.headers.len() {
            clean_cell(row.cells.get(index).map_or("", String::as_str))
        } else {
            clean_cell(&row.note)
        }
    }

    /// Whether any row carries an exceptional note, so boxed panels only grow
    /// a Note column when there is something to show.
    pub(super) fn has_note(&self) -> bool {
        self.sections
            .iter()
            .flat_map(|section| &section.rows)
            .any(|row| !row.note.is_empty())
    }

    /// Boxed headers: the base columns plus a `Note` column when needed.
    pub(super) fn boxed_headers(&self) -> Vec<String> {
        let mut headers: Vec<String> = self.headers.iter().map(|label| clean_cell(label)).collect();
        if self.has_note() {
            headers.push("Note".into());
        }
        headers
    }

    /// Why a plan row is present: the dependency sections always mean a
    /// dependency; otherwise the row's own request flag decides.
    pub(super) fn reason_for(section: &Section, row: &Row) -> &'static str {
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
