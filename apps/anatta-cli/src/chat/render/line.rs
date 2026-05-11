//! Line-mode renderer for `anatta chat`.
//!
//! Strategy:
//!   * TTY mode: paint each `*Delta` in place (rewind + clear + repaint
//!     via crossterm cursor ops). The matching final event is a no-op
//!     because the screen is already correct.
//!   * Non-TTY mode: skip deltas; paint finals only as plain text.
//!
//! Only one streaming region is "open" at any moment (deltas of one
//! content block stream contiguously). `last_delta` tracks the
//! rewindable region; any other paint commits it.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};

use anatta_core::{AgentEvent, AgentEventPayload};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use serde_json::Value;
use termimad::MadSkin;
use unicode_width::UnicodeWidthStr;

use super::{markdown, EventRenderer, PALETTE};

const DEFAULT_WIDTH: u16 = 100;

pub(crate) struct LineRenderer {
    is_tty: bool,
    skin: MadSkin,
    width: u16,
    last_delta: Option<DeltaAnchor>,
    /// tool_use_ids that have seen a `ToolUseInputDelta` but no final
    /// `ToolUse` yet. Flushed on TurnCompleted so non-TTY consumers see
    /// at least a placeholder when a final never arrives (e.g. turn
    /// aborted before claude's assistant message_stop).
    pending_tool_ids: HashSet<String>,
}

struct DeltaAnchor {
    block_index: u32,
    kind: DeltaKind,
    /// How many terminal rows the previous paint of this region occupied.
    lines: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeltaKind {
    Text,
    Thinking,
}

impl LineRenderer {
    pub(crate) fn new() -> Self {
        let is_tty = io::stdout().is_terminal();
        let width = if is_tty {
            terminal::size().map(|(c, _)| c).unwrap_or(DEFAULT_WIDTH)
        } else {
            DEFAULT_WIDTH
        };
        Self {
            is_tty,
            skin: markdown::build_skin(),
            width,
            last_delta: None,
            pending_tool_ids: HashSet::new(),
        }
    }

    fn commit_delta(&mut self) {
        // A non-delta event ended the streaming region. The painted
        // content stays on screen; clear our anchor so the next event
        // paints fresh below it.
        self.last_delta = None;
    }

    /// Emit a placeholder line for each tool_use_id that saw a delta
    /// but never received a final ToolUse. Called on TurnCompleted /
    /// chat end. Drained after, so subsequent turns start fresh.
    fn flush_pending_tools(&mut self) {
        if self.pending_tool_ids.is_empty() {
            return;
        }
        let ids: Vec<String> = self.pending_tool_ids.drain().collect();
        for id in ids {
            let short = if id.len() > 12 { &id[..12] } else { id.as_str() };
            let line = format!("⚙ <unfinalized tool, id={short}…>");
            let painted = if self.is_tty {
                format!("{}\n", color(PALETTE.tool, &line))
            } else {
                format!("{line}\n")
            };
            self.print(&painted);
        }
    }

    fn rewind_lines(&mut self, lines: u16) -> io::Result<()> {
        let mut out = io::stdout().lock();
        if lines > 0 {
            queue!(out, cursor::MoveToColumn(0))?;
            queue!(out, cursor::MoveUp(lines))?;
        } else {
            queue!(out, cursor::MoveToColumn(0))?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
        out.flush()
    }

    fn paint_text_block(&mut self, idx: u32, text: &str, kind: DeltaKind) {
        let painted = match kind {
            DeltaKind::Text => self.render_assistant(text),
            DeltaKind::Thinking => self.render_thinking(text),
        };
        let lines = count_lines(&painted, self.width);
        let mut out = io::stdout().lock();
        let _ = out.write_all(painted.as_bytes());
        if !painted.ends_with('\n') {
            let _ = out.write_all(b"\n");
        }
        let _ = out.flush();
        drop(out);
        self.last_delta = Some(DeltaAnchor {
            block_index: idx,
            kind,
            lines,
        });
    }

    fn handle_delta(&mut self, idx: u32, text_so_far: &str, kind: DeltaKind) {
        if !self.is_tty {
            return;
        }
        // If the previous delta was the same block + kind, rewind to its
        // start before repainting. Otherwise this is a fresh region; the
        // earlier region is committed where it is.
        let rewind = match &self.last_delta {
            Some(prev) if prev.block_index == idx && prev.kind == kind => Some(prev.lines),
            _ => None,
        };
        if let Some(lines) = rewind {
            let _ = self.rewind_lines(lines);
        }
        self.paint_text_block(idx, text_so_far, kind);
    }

    fn handle_final_text(&mut self, text: &str, kind: DeltaKind) {
        // If a matching delta region is still open, the text is already
        // painted — just close it. Otherwise paint fresh.
        let already_painted = matches!(
            &self.last_delta,
            Some(a) if a.kind == kind,
        );
        if already_painted {
            self.commit_delta();
            return;
        }
        // No prior delta (non-TTY, or projector emitted only the final):
        // paint as a fresh region without recording an anchor.
        let painted = match kind {
            DeltaKind::Text => self.render_assistant(text),
            DeltaKind::Thinking => self.render_thinking(text),
        };
        let mut out = io::stdout().lock();
        let _ = out.write_all(painted.as_bytes());
        if !painted.ends_with('\n') {
            let _ = out.write_all(b"\n");
        }
        let _ = out.flush();
    }

    fn render_assistant(&self, text: &str) -> String {
        if self.is_tty {
            markdown::render(&self.skin, text, self.width as usize)
        } else {
            // Non-tty: emit raw markdown source, no ANSI.
            let mut s = text.to_owned();
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s
        }
    }

    fn render_thinking(&self, text: &str) -> String {
        let mut out = String::new();
        let prefix_color = PALETTE.thinking;
        for line in text.lines() {
            if self.is_tty {
                out.push_str(&color(prefix_color, "│ "));
                out.push_str(&color(prefix_color, line));
                out.push('\n');
            } else {
                out.push_str("│ ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if out.is_empty() {
            out.push('\n');
        }
        out
    }

    fn render_tool_use(&self, name: &str, input: &Value) -> String {
        let summary = tool_input_summary(input);
        let line = format!("⚙ {name}({summary})");
        if self.is_tty {
            format!("{}\n", color(PALETTE.tool, &line))
        } else {
            format!("{line}\n")
        }
    }

    fn render_tool_result(&self, success: bool, text: Option<&str>, structured: Option<&Value>) -> String {
        let summary = result_summary(text, structured);
        let glyph = if success { "✓" } else { "✗" };
        let color_c = if success { PALETTE.tool_ok } else { PALETTE.tool_err };
        let main = format!("  {glyph} {summary}");
        let mut s = if self.is_tty {
            format!("{}\n", color(color_c, &main))
        } else {
            format!("{main}\n")
        };
        // Long-text preview: render up to 4 indented dim lines + "(M more)".
        if let Some(t) = text {
            if t.len() > 200 {
                let lines: Vec<&str> = t.lines().collect();
                let head = lines.iter().take(4).copied().collect::<Vec<_>>();
                for line in &head {
                    if self.is_tty {
                        s.push_str(&color(PALETTE.usage, &format!("    {line}")));
                    } else {
                        s.push_str(&format!("    {line}"));
                    }
                    s.push('\n');
                }
                if lines.len() > head.len() {
                    let remaining = lines.len() - head.len();
                    let tail = format!("    …({remaining} lines more)");
                    if self.is_tty {
                        s.push_str(&color(PALETTE.usage, &tail));
                    } else {
                        s.push_str(&tail);
                    }
                    s.push('\n');
                }
            }
        }
        s
    }

    fn render_usage(&self, input_tokens: u64, output_tokens: u64, cost: Option<f64>) -> String {
        let line = match cost {
            Some(c) => format!(
                "· {} in · {} out · ${c:.4}",
                humanize_tokens(input_tokens),
                humanize_tokens(output_tokens),
            ),
            None => format!(
                "· {} in · {} out",
                humanize_tokens(input_tokens),
                humanize_tokens(output_tokens),
            ),
        };
        if self.is_tty {
            format!("{}\n", color(PALETTE.usage, &line))
        } else {
            format!("{line}\n")
        }
    }

    fn render_separator(&self) -> String {
        let w = self.width.max(20) as usize;
        let line = "─".repeat(w);
        if self.is_tty {
            format!("{}\n\n", color(PALETTE.separator, &line))
        } else {
            format!("{line}\n\n")
        }
    }

    fn render_rate_limit(&self, kind: &str, resets_at: Option<i64>) -> String {
        let when = match resets_at {
            Some(ts) => chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| ts.to_string()),
            None => "unknown".into(),
        };
        let line = format!("⚠ rate limit ({kind}) — resets at {when}");
        if self.is_tty {
            format!("{}\n", color(PALETTE.rate_limit, &line))
        } else {
            format!("{line}\n")
        }
    }

    fn render_error(&self, message: &str, fatal: bool) -> String {
        let mut s = if self.is_tty {
            format!("{}\n", color(PALETTE.error, &format!("✗ error: {message}")))
        } else {
            format!("✗ error: {message}\n")
        };
        if fatal {
            let trail = "session terminated";
            if self.is_tty {
                s.push_str(&color(PALETTE.error, trail));
                s.push('\n');
            } else {
                s.push_str(trail);
                s.push('\n');
            }
        }
        s
    }

    fn print(&mut self, s: &str) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}

impl EventRenderer for LineRenderer {
    fn on_event(&mut self, ev: &AgentEvent) {
        match &ev.payload {
            AgentEventPayload::SessionStarted { .. }
            | AgentEventPayload::TurnStarted
            | AgentEventPayload::UserPrompt { .. } => {}

            AgentEventPayload::AssistantTextDelta {
                content_block_index,
                text_so_far,
            } => self.handle_delta(*content_block_index, text_so_far, DeltaKind::Text),

            AgentEventPayload::AssistantText { text } => {
                self.handle_final_text(text, DeltaKind::Text);
            }

            AgentEventPayload::ThinkingDelta {
                content_block_index,
                text_so_far,
            } => self.handle_delta(*content_block_index, text_so_far, DeltaKind::Thinking),

            AgentEventPayload::Thinking { text } => {
                self.handle_final_text(text, DeltaKind::Thinking);
            }

            AgentEventPayload::ToolUse { id, name, input } => {
                self.commit_delta();
                self.pending_tool_ids.remove(id);
                let s = self.render_tool_use(name, input);
                self.print(&s);
            }

            AgentEventPayload::ToolUseInputDelta { tool_use_id, .. } => {
                // Track that a tool block is in progress. On TurnCompleted
                // any still-pending ids get a placeholder line — protects
                // non-TTY logs against aborted turns where the final
                // ToolUse never arrives.
                self.pending_tool_ids.insert(tool_use_id.clone());
            }

            AgentEventPayload::ToolResult {
                success,
                text,
                structured,
                ..
            } => {
                self.commit_delta();
                let s = self.render_tool_result(*success, text.as_deref(), structured.as_ref());
                self.print(&s);
            }

            AgentEventPayload::Usage {
                input_tokens,
                output_tokens,
                cost_usd,
                ..
            } => {
                self.commit_delta();
                let s = self.render_usage(*input_tokens, *output_tokens, *cost_usd);
                self.print(&s);
            }

            AgentEventPayload::TurnCompleted { .. } => {
                self.commit_delta();
                self.flush_pending_tools();
                let s = self.render_separator();
                self.print(&s);
            }

            AgentEventPayload::RateLimit {
                limit_kind,
                resets_at,
                ..
            } => {
                self.commit_delta();
                let s = self.render_rate_limit(limit_kind, *resets_at);
                self.print(&s);
            }

            AgentEventPayload::Error { message, fatal } => {
                self.commit_delta();
                let s = self.render_error(message, *fatal);
                self.print(&s);
            }
        }
    }

    fn on_turn_end(&mut self) {
        self.commit_delta();
    }

    fn on_chat_end(&mut self) {
        // Flush any pending tools from the last in-flight turn so
        // chat-end without a TurnCompleted still leaves coverage.
        self.flush_pending_tools();
        if self.is_tty {
            let mut out = io::stdout().lock();
            let _ = queue!(out, ResetColor);
            let _ = out.flush();
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// helpers
// ──────────────────────────────────────────────────────────────────────

fn color(c: Color, s: &str) -> String {
    let mut out = String::new();
    use std::fmt::Write as _;
    // crossterm style escapes via Display
    let _ = write!(out, "{}{}{}", SetForegroundColor(c), s, ResetColor);
    out
}

/// Count how many terminal rows `s` occupies given `width`. Counts each
/// `\n` as a row break and accounts for wide lines wrapping.
fn count_lines(s: &str, width: u16) -> u16 {
    let w = width.max(1) as usize;
    let mut rows: u32 = 0;
    for line in s.split('\n') {
        let display_w = UnicodeWidthStr::width(line);
        let chunks = display_w.div_ceil(w).max(1);
        rows = rows.saturating_add(chunks as u32);
    }
    // s.split('\n') over "a\n" yields ["a", ""]; the trailing empty
    // counted one extra row — subtract it.
    if s.ends_with('\n') {
        rows = rows.saturating_sub(1);
    }
    rows.try_into().unwrap_or(u16::MAX)
}

/// Format tool input: prefer first two object fields with truncated values.
fn tool_input_summary(v: &Value) -> String {
    if let Value::Object(map) = v {
        let mut parts: Vec<String> = Vec::with_capacity(2);
        for (k, val) in map.iter().take(2) {
            let val_str = match val {
                Value::String(s) => format!("\"{}\"", truncate(s, 60)),
                other => truncate(&other.to_string(), 60),
            };
            parts.push(format!("{k}={val_str}"));
        }
        let mut s = parts.join(", ");
        if map.len() > 2 {
            s.push_str(&format!(", …+{} fields", map.len() - 2));
        }
        s
    } else {
        truncate(&v.to_string(), 100)
    }
}

fn result_summary(text: Option<&str>, structured: Option<&Value>) -> String {
    if let Some(t) = text {
        if t.is_empty() {
            "(empty)".into()
        } else {
            let first_line = t.lines().next().unwrap_or("");
            truncate(first_line, 80)
        }
    } else if let Some(v) = structured {
        truncate(&v.to_string(), 80)
    } else {
        "(no output)".into()
    }
}

fn humanize_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_owned()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("héllo", 10), "héllo");
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    #[test]
    fn tool_input_summary_two_fields_then_count() {
        let v = serde_json::json!({"a": 1, "b": 2, "c": 3, "d": 4});
        let s = tool_input_summary(&v);
        assert!(s.contains("a=1"));
        assert!(s.contains("…+2 fields"));
    }

    #[test]
    fn tool_input_summary_truncates_long_string() {
        let v = serde_json::json!({"command": "a".repeat(100)});
        let s = tool_input_summary(&v);
        assert!(s.len() < 130);
        assert!(s.contains("…"));
    }

    #[test]
    fn result_summary_falls_back() {
        assert_eq!(result_summary(None, None), "(no output)");
        assert_eq!(result_summary(Some(""), None), "(empty)");
        assert_eq!(result_summary(Some("ok"), None), "ok");
    }

    #[test]
    fn humanize_tokens_scales() {
        assert_eq!(humanize_tokens(50), "50");
        assert_eq!(humanize_tokens(1_500), "1.5k");
        assert_eq!(humanize_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn count_lines_handles_trailing_newline() {
        // Three full lines, width 100.
        assert_eq!(count_lines("a\nb\nc", 100), 3);
        assert_eq!(count_lines("a\nb\nc\n", 100), 3);
        // Wrapping: 110 chars at width 100 = 2 rows.
        let s = "x".repeat(110);
        assert_eq!(count_lines(&s, 100), 2);
    }
}
