//! Terminal and panel width.

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
