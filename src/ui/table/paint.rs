//! Painting a laid-out panel into a ratatui buffer and serialising it to ANSI.

use console::measure_text_width as cells;
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
    text::{Line, Span},
    widgets::{Block, BorderType, Padding, Widget},
};

use super::format::FIELD_GAP;
use super::layout::{Layout, PRow};
use super::tone::{RESET, Tone, sgr};

impl Layout {
    pub(super) fn paint(&self, buffer: &mut Buffer) {
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

pub(super) fn buffer_to_string(buffer: &Buffer, colored: bool) -> String {
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
