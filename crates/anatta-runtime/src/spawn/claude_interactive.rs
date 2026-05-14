//! Interactive `claude` (no `--print`) running inside a PTY.
//!
//! claude refuses to start without a tty, so we allocate one — but the
//! TUI bytes are discarded. The real data path is the session JSONL at
//! `<CLAUDE_CONFIG_DIR>/projects/<encoded-cwd>/<session-uuid>.jsonl`,
//! which we tail with [`HistoryProjector`](crate::claude::HistoryProjector).

/// Wrap `prompt` in xterm bracketed-paste escape sequences and terminate
/// with a CR (which claude's input handler interprets as "submit").
///
/// Bracketed paste tells claude these bytes are pasted content, not
/// typed — which preserves embedded newlines as literal newlines instead
/// of treating them as submit keystrokes mid-prompt.
pub(crate) fn encode_prompt(prompt: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(prompt.len() + 13);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(prompt.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out.push(b'\r');
    out
}

#[doc(hidden)]
pub fn encode_prompt_for_test(prompt: &str) -> Vec<u8> {
    encode_prompt(prompt)
}
