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

mod format;
mod layout;
mod mode;
mod model;
mod paint;
mod plain;
mod render;
mod tone;
mod width;

pub use format::{clean_cell, display_path, field, fields, format_error, result};
pub use mode::Mode;
pub use model::{Panel, Row, Section};
// Part of the module's public surface even though no caller outside the
// table's own tests uses them today.
pub use width::{panel_width, terminal_width};
#[allow(unused_imports)]
pub use {format::summary_block, mode::ModeInputs, tone::bold};

#[cfg(test)]
mod tests;
