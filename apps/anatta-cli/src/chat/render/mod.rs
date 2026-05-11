//! Rendering layer for the chat REPL.
//!
//! `EventRenderer` is the CLI-internal trait every backend (line-mode
//! today, ratatui later) implements. The chat loop holds
//! `&mut dyn EventRenderer` and never touches rendering primitives
//! directly. termimad is encapsulated under [`markdown`] so swapping
//! markdown engines doesn't ripple into [`line`].

use anatta_core::AgentEvent;
use crossterm::style::Color;

pub(crate) mod line;
pub(crate) mod markdown;

/// Renderer contract. Implementations decide how (and whether) to render
/// each lifecycle hook; `on_event` dispatches across `AgentEventPayload`.
pub(crate) trait EventRenderer {
    fn on_event(&mut self, ev: &AgentEvent);

    /// Called after one turn ends (the child exited for one-shot
    /// backends like claude, or `turn/completed` fired for persistent
    /// backends like codex). Renderers use this to commit pending
    /// delta state.
    fn on_turn_end(&mut self);

    /// Called right before the chat loop reads the next prompt. Lets
    /// renderers print a sticky status line (rate-limit summary, etc.)
    /// above the `>` prompt without cluttering the inline transcript
    /// while a turn is streaming.
    fn pre_prompt(&mut self);

    /// Called once before the chat loop returns. Last chance to flush
    /// state, render a goodbye line, etc.
    fn on_chat_end(&mut self);
}

/// Renderer color palette. Shared by [`line`] and [`markdown`] so the
/// markdown styling matches the rest of the output.
pub(crate) struct Palette {
    pub thinking: Color,
    pub tool: Color,
    pub tool_ok: Color,
    pub tool_err: Color,
    pub error: Color,
    pub rate_limit: Color,
    pub usage: Color,
    pub separator: Color,
}

pub(crate) const PALETTE: Palette = Palette {
    thinking: Color::DarkGrey,
    tool: Color::Cyan,
    tool_ok: Color::Green,
    tool_err: Color::Red,
    error: Color::Red,
    rate_limit: Color::Yellow,
    usage: Color::DarkGrey,
    separator: Color::DarkGrey,
};
