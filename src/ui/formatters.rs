use colored::{ColoredString, Colorize};

use super::table::{self, Mode, Panel, Section, clean_cell};
use crate::config::Config;

pub struct DnfStyleFormatter {
    mode: Mode,
}

/// A rendered table and, when the panel carries one, its summary block.
///
/// In plain mode the summary is kept out of [`table`](Self::table) so a caller
/// can print it to stderr; in boxed mode it is already drawn inside the panel
/// and `summary` is `None`.
pub struct Rendered {
    pub table: String,
    pub summary: Option<String>,
}

impl DnfStyleFormatter {
    pub fn new(config: &Config) -> Self {
        Self {
            mode: Mode::detect(config.ui.colored),
        }
    }

    pub fn header(&self, text: &str) -> String {
        self.section(text)
    }

    pub fn section(&self, title: &str) -> String {
        let title = clean_cell(title);
        if self.mode.colored {
            title.cyan().bold().to_string()
        } else {
            title
        }
    }

    /// One indented `Label    value` line (continuation lines align under the
    /// value). Labels are dim, values normal.
    pub fn field(&self, label: &str, value: &str) -> String {
        table::field(self.mode, label, value)
    }

    /// Outcome line, e.g. `✔ Installed 1 formula`.
    pub fn result(&self, text: &str) -> String {
        table::result(self.mode, text)
    }

    /// Follow-up fields aligned to each other under an outcome line.
    pub fn fields(&self, rows: &[(&str, String)]) -> String {
        table::fields(self.mode, rows)
    }

    /// Whether tables are drawn as panels (otherwise plain columns).
    pub fn boxed(&self) -> bool {
        self.mode.boxed
    }

    /// A titled table: a rounded panel on a terminal, plain aligned columns
    /// when redirected.
    pub fn table(&self, title: &str, headers: &[&str], rows: &[Vec<String>]) -> String {
        self.panel(&Panel::flat(title, headers, rows))
    }

    /// A flat table closed by a `Summary` section.
    pub fn table_with_summary(
        &self,
        title: &str,
        headers: &[&str],
        rows: &[Vec<String>],
        summary: &[(&str, String)],
    ) -> Rendered {
        self.rendered(&Panel::flat(title, headers, rows).with_summary(summary))
    }

    /// A panel whose rows are grouped under dnf-style labels and which ends
    /// with a `Summary` section.
    pub fn plan_table(
        &self,
        title: &str,
        headers: &[&str],
        sections: Vec<Section>,
        summary: &[(&str, String)],
    ) -> Rendered {
        self.rendered(
            &Panel {
                title,
                headers,
                sections,
                summary: Vec::new(),
            }
            .with_summary(summary),
        )
    }

    fn panel(&self, panel: &Panel) -> String {
        let width = if self.mode.boxed {
            Some(table::panel_width(table::terminal_width()))
        } else {
            None
        };
        panel.render(self.mode, width)
    }

    fn rendered(&self, panel: &Panel) -> Rendered {
        let width = if self.mode.boxed {
            Some(table::panel_width(table::terminal_width()))
        } else {
            None
        };
        let table = panel.render(self.mode, width);
        let summary =
            (!self.mode.boxed && !panel.summary.is_empty()).then(|| panel.plain_summary(self.mode));
        Rendered { table, summary }
    }

    /// Print a closing note to stdout when styled and to stderr when plain, so
    /// redirected stdout carries only the machine-readable table.
    pub fn note(&self, text: &str) {
        if self.mode.boxed {
            println!("{text}");
        } else {
            eprintln!("{text}");
        }
    }

    pub fn warning(&self, text: &str) -> ColoredString {
        if self.mode.colored {
            text.yellow()
        } else {
            text.normal()
        }
    }

    pub fn success(&self, text: &str) -> ColoredString {
        if self.mode.colored {
            text.green()
        } else {
            text.normal()
        }
    }

    pub fn info(&self, text: &str) -> ColoredString {
        if self.mode.colored {
            text.cyan()
        } else {
            text.normal()
        }
    }

    pub fn format_size(&self, bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
        let mut size = bytes as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit + 1 < UNITS.len() {
            size /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} B")
        } else {
            format!("{size:.1} {}", UNITS[unit])
        }
    }
}
