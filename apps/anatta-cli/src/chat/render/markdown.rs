//! AssistantText markdown rendering. termimad is invoked from here only.
//!
//! The skin is constructed once per `LineRenderer` (in [`build_skin`])
//! with palette-aligned colors. [`render`] is the single entry point —
//! it returns a String of ANSI-escaped text the caller writes verbatim
//! to the terminal.

use crossterm::style::Color;
use termimad::MadSkin;

use super::PALETTE;

pub(super) fn build_skin() -> MadSkin {
    let mut skin = MadSkin::default();
    // termimad re-exports crossterm internally; with matched versions
    // these types are identical, so direct usage works.
    skin.bold.set_fg(Color::White);
    skin.italic.set_fg(Color::White);
    skin.inline_code.set_fg(PALETTE.tool);
    skin.code_block.set_bg(Color::AnsiValue(236));
    for header in skin.headers.iter_mut() {
        header.set_fg(Color::White);
    }
    skin.bullet.set_fg(PALETTE.thinking);
    skin.quote_mark.set_fg(PALETTE.thinking);
    skin.horizontal_rule.set_fg(PALETTE.separator);
    skin
}

/// Render `text` (markdown source) into an ANSI-escaped string sized to
/// `width` columns. termimad handles wrapping, tables, code blocks.
pub(super) fn render(skin: &MadSkin, text: &str, width: usize) -> String {
    skin.text(text, Some(width)).to_string()
}
