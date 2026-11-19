use colored::{ColoredString, Colorize};
use console::measure_text_width as cells;

use crate::config::Config;

fn clean_cell(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub struct DnfStyleFormatter {
    use_colors: bool,
}

impl DnfStyleFormatter {
    pub fn new(config: &Config) -> Self {
        Self {
            use_colors: config.use_colors(),
        }
    }

    pub fn header(&self, text: &str) -> String {
        self.section(text)
    }

    pub fn section(&self, title: &str) -> String {
        let title = clean_cell(title);
        if self.use_colors {
            title.bright_blue().bold().to_string()
        } else {
            title
        }
    }

    pub fn field(&self, label: &str, value: &str) -> String {
        let padding = 12usize.saturating_sub(cells(label));
        let mut lines = value.split('\n').map(clean_cell);
        let mut result = format!(
            "{}{:padding$} : {}",
            self.section(label),
            "",
            lines.next().unwrap_or_default()
        );
        let indent = " ".repeat(cells(label).max(12) + 3);
        for line in lines {
            result.push('\n');
            result.push_str(&indent);
            result.push_str(&line);
        }
        result
    }

    /// Compact, width-aware terminal columns; plain text when redirected.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        let term = console::Term::stdout();
        let width = (term.is_term()
            && std::env::var_os("TERM").as_deref() != Some("dumb".as_ref()))
        .then(|| usize::from(term.size().1))
        .filter(|width| *width > 0);
        self.render_table(headers, rows, width)
    }

    fn render_table(&self, headers: &[&str], rows: &[Vec<String>], width: Option<usize>) -> String {
        if headers.is_empty() {
            return String::new();
        }
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                (0..headers.len())
                    .map(|index| {
                        row.get(index)
                            .map_or_else(String::new, |cell| clean_cell(cell))
                    })
                    .collect()
            })
            .collect();
        let mut widths: Vec<_> = headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                rows.iter()
                    .map(|row| cells(&row[index]))
                    .fold(cells(header), usize::max)
            })
            .collect();

        let Some(width) = width else {
            return plain_table(headers, &rows, &widths);
        };
        // Shrink prose/URLs before falling back to stacked fields. Never
        // abbreviate installable targets.
        let gutter = 2 * (widths.len() - 1);
        let mut over = (widths.iter().sum::<usize>() + gutter).saturating_sub(width);
        for (label, column) in headers.iter().zip(&mut widths).rev() {
            if is_flexible(label) {
                let reclaimed = over.min(column.saturating_sub(cells(label).max(16)));
                *column -= reclaimed;
                over -= reclaimed;
            }
        }
        if over > 0 && !rows.is_empty() {
            return rows
                .iter()
                .map(|row| {
                    let mut fields = vec![self.section(&row[0])];
                    fields.extend(
                        headers
                            .iter()
                            .zip(row)
                            .skip(1)
                            .map(|(label, value)| self.field(label, value)),
                    );
                    fields.join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n\n");
        }

        let row_line = |values: &[&str], heading: bool| {
            let mut line = String::new();
            for (index, (value, width)) in values.iter().zip(&widths).enumerate() {
                line.push_str(&self.style_cell(headers[index], index, value, heading));
                if index + 1 < widths.len()
                    && values[index + 1..].iter().any(|value| !value.is_empty())
                {
                    let padding = width.saturating_sub(cells(value)) + 2;
                    line.push_str(&" ".repeat(padding));
                }
            }
            line
        };
        let rule = "─".repeat(widths.iter().sum::<usize>() + gutter);
        let mut output = vec![
            row_line(headers, true),
            if self.use_colors {
                rule.dimmed().to_string()
            } else {
                rule
            },
        ];
        for row in &rows {
            let wrapped: Vec<_> = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| wrap_cell(cell, *width))
                .collect();
            for line in 0..wrapped.iter().map(Vec::len).max().unwrap_or(0) {
                let values: Vec<_> = wrapped
                    .iter()
                    .map(|column| column.get(line).map_or("", String::as_str))
                    .collect();
                output.push(row_line(&values, false));
            }
        }
        output.join("\n")
    }

    fn style_cell(&self, label: &str, index: usize, value: &str, heading: bool) -> String {
        if !self.use_colors || value.is_empty() {
            return value.into();
        }
        if heading {
            return value.bright_blue().bold().to_string();
        }
        if index == 0 {
            return value.bold().to_string();
        }
        match (label, value.to_ascii_lowercase().as_str()) {
            ("Type", "cask") => value.magenta().to_string(),
            ("Type", "formula") => value.cyan().to_string(),
            ("Status", "pinned") => value.yellow().to_string(),
            ("Status", "ready") => value.green().to_string(),
            ("Tap" | "Repository" | "Homepage", _) => value.dimmed().to_string(),
            _ => value.into(),
        }
    }

    pub fn warning(&self, text: &str) -> ColoredString {
        if self.use_colors {
            text.yellow()
        } else {
            text.normal()
        }
    }

    pub fn success(&self, text: &str) -> ColoredString {
        if self.use_colors {
            text.green()
        } else {
            text.normal()
        }
    }

    pub fn info(&self, text: &str) -> ColoredString {
        if self.use_colors {
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

fn plain_table(headers: &[&str], rows: &[Vec<String>], widths: &[usize]) -> String {
    let row_line = |values: &[&str]| {
        values
            .iter()
            .zip(widths)
            .enumerate()
            .map(|(index, (value, width))| {
                if index + 1 == headers.len() {
                    value.to_string()
                } else {
                    let padding = width.saturating_sub(cells(value)) + 2;
                    format!("{value}{:padding$}", "")
                }
            })
            .collect::<String>()
    };
    let mut output = vec![
        row_line(headers),
        "-".repeat(widths.iter().sum::<usize>() + 2 * (widths.len() - 1)),
    ];
    for row in rows {
        output.push(row_line(
            &row.iter().map(String::as_str).collect::<Vec<_>>(),
        ));
    }
    output.join("\n")
}

fn is_flexible(label: &str) -> bool {
    matches!(label, "Description" | "Summary" | "Homepage")
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

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATTER: DnfStyleFormatter = DnfStyleFormatter { use_colors: false };

    #[test]
    fn pipe_output_aligns_wide_glyphs_and_sanitizes_values() {
        let rendered = FORMATTER.render_table(
            &["Package", "Version"],
            &[
                vec!["猫".into(), "1\n2".into()],
                vec!["a".into(), "3".into()],
            ],
            None,
        );
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines[2], "猫       1 2");
        assert_eq!(lines[3], "a        3");
    }

    #[test]
    fn terminal_wraps_descriptions_without_borders() {
        let rendered = FORMATTER.render_table(
            &["Package", "Version", "Description"],
            &[vec![
                "firefox@beta".into(),
                "128.0".into(),
                "Mozilla Firefox Beta web browser".into(),
            ]],
            Some(40),
        );
        assert!(rendered.starts_with("Package"));
        assert!(rendered.contains("firefox@beta"));
        assert!(rendered.contains("web browser"));
        assert!(!rendered.contains('│'));
        assert!(rendered.lines().all(|line| cells(line) <= 40));
    }

    #[test]
    fn narrow_output_keeps_full_package_names() {
        let rendered = FORMATTER.render_table(
            &["Package", "Available"],
            &[vec![
                "example/tap/very-long-package-name".into(),
                "2.0".into(),
            ]],
            Some(24),
        );
        assert_eq!(
            rendered,
            "example/tap/very-long-package-name\nAvailable    : 2.0"
        );
    }
}
