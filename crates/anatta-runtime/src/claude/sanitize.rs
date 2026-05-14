//! StripReasoning sanitizer for claude session JSONL.
//!
//! When a conversation segment moves from a lax family (a-compat) to a
//! strict family (a-native), each prior segment's events must be rendered
//! into the new working file with thinking blocks removed. Anthropic
//! validates thinking-block signatures server-side; 3rd-party proxies
//! produce content that lacks valid signatures, and passing it back to
//! Anthropic causes a 400.
//!
//! ## Algorithm
//!
//! Empirically validated across 5 real claude session JSONLs (covering
//! 657 thinking-only events): every thinking-only assistant event has
//! exactly **one** parentUuid child and is never adjacent to another
//! thinking-only event. The sanitizer relies on this invariant to perform
//! a single-child linked-list splice:
//!
//! 1. Identify thinking-only assistant events.
//! 2. For each such event T with parent P: rewrite T's single child C's
//!    `parentUuid` from T's uuid → P's uuid.
//! 3. Emit all events except the dropped T's.
//!
//! ## Defensive fallback
//!
//! If a thinking event has != 1 children, or its parent is also
//! thinking-only, the sanitizer cannot safely splice. It falls back to
//! "keep the event but blank out the thinking text" — preserving the
//! DAG topology, losing only the (invalid) thinking content.
//!
//! ## Why `serde_json::Value`, not typed `ClaudeEvent`
//!
//! `crates/anatta-runtime/src/claude/history.rs` defines strict tagged
//! enums for parsing. They serialize back faithfully for known fields
//! but **lose** unknown extras (claude adds fields between releases).
//! The sanitizer must not silently drop fields — we round-trip via
//! `serde_json::Value` to preserve every byte we don't intentionally
//! touch.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum SanitizeError {
    #[error("malformed JSONL line: {line}\n  reason: {source}")]
    Parse {
        line: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Sanitize a stream of claude JSONL events: drop thinking-only assistant
/// events and rewire the DAG. Unknown fields are preserved verbatim.
pub fn strip_reasoning<R: BufRead, W: Write>(src: R, mut dst: W) -> Result<(), SanitizeError> {
    let mut lines: Vec<Parsed> = Vec::new();

    // Pass 1: parse all events.
    for line in src.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|e| SanitizeError::Parse {
            line: line.clone(),
            source: e,
        })?;
        let uuid = value.get("uuid").and_then(Value::as_str).map(String::from);
        let parent_uuid = value
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(String::from);
        let is_thinking_only = is_thinking_only_assistant(&value);
        lines.push(Parsed {
            value,
            uuid,
            parent_uuid,
            is_thinking_only,
        });
    }

    // Pass 2: identify thinking events to drop / blank.
    //
    // child_count[parent_uuid] counts how many events claim this parent.
    let mut child_count: HashMap<String, u32> = HashMap::new();
    for p in &lines {
        if let Some(parent) = &p.parent_uuid {
            *child_count.entry(parent.clone()).or_insert(0) += 1;
        }
    }

    // For each thinking-only event, decide: drop+splice OR blank+keep.
    // We must take the decision in an immutable pass first, then apply
    // any in-place blanking, to satisfy the borrow checker.
    let mut drop_uuids: HashSet<String> = HashSet::new();
    let mut new_parent_for_dropped: HashMap<String, Option<String>> = HashMap::new();
    let mut blank_indices: Vec<usize> = Vec::new();
    let parent_thinking_only: HashSet<String> = lines
        .iter()
        .filter(|p| p.is_thinking_only)
        .filter_map(|p| p.uuid.clone())
        .collect();

    for (i, p) in lines.iter().enumerate() {
        if !p.is_thinking_only {
            continue;
        }
        let Some(uuid) = p.uuid.clone() else {
            // Defensive: thinking event without a uuid. Blank it in place.
            blank_indices.push(i);
            continue;
        };
        let n_children = child_count.get(&uuid).copied().unwrap_or(0);
        let parent_is_thinking = p
            .parent_uuid
            .as_deref()
            .is_some_and(|pu| parent_thinking_only.contains(pu));
        // Conservative invariant check: also confirm the single child is
        // NOT another thinking event. If either neighbour is thinking,
        // fall back to blank-in-place rather than relinking through it.
        let child_is_thinking = if n_children == 1 {
            let single_child = lines
                .iter()
                .find(|q| q.parent_uuid.as_deref() == Some(&uuid));
            single_child.map(|q| q.is_thinking_only).unwrap_or(false)
        } else {
            false
        };
        if n_children == 1 && !parent_is_thinking && !child_is_thinking {
            drop_uuids.insert(uuid.clone());
            new_parent_for_dropped.insert(uuid, p.parent_uuid.clone());
        } else {
            blank_indices.push(i);
        }
    }

    for i in blank_indices {
        blank_thinking_in_place(&mut lines[i].value);
    }

    // Pass 3: emit non-dropped events, rewriting parentUuid where needed.
    for line in &lines {
        if let Some(u) = &line.uuid {
            if drop_uuids.contains(u) {
                continue;
            }
        }
        let mut out = line.value.clone();
        if let Some(p) = &line.parent_uuid {
            if let Some(new_p) = new_parent_for_dropped.get(p) {
                if let Some(obj) = out.as_object_mut() {
                    match new_p {
                        Some(np) => obj.insert("parentUuid".into(), Value::String(np.clone())),
                        None => obj.insert("parentUuid".into(), Value::Null),
                    };
                }
            }
        }
        let s = serde_json::to_string(&out)?;
        dst.write_all(s.as_bytes())?;
        dst.write_all(b"\n")?;
    }
    dst.flush()?;
    Ok(())
}

struct Parsed {
    value: Value,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    is_thinking_only: bool,
}

/// True iff this is an assistant event whose `message.content[]` consists
/// entirely of `thinking` blocks (and has at least one).
fn is_thinking_only_assistant(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if content.is_empty() {
        return false;
    }
    content
        .iter()
        .all(|b| b.get("type").and_then(Value::as_str) == Some("thinking"))
}

/// Set every `thinking` block's `thinking` field to an empty string and
/// drop its `signature` field. The event keeps its uuid / parentUuid so
/// the DAG is preserved.
fn blank_thinking_in_place(v: &mut Value) {
    let Some(content) = v
        .get_mut("message")
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for blk in content {
        let Some(obj) = blk.as_object_mut() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) == Some("thinking") {
            obj.insert("thinking".into(), Value::String(String::new()));
            obj.remove("signature");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sanitize_str(input: &str) -> String {
        let mut out = Vec::new();
        strip_reasoning(Cursor::new(input), &mut out).expect("sanitize");
        String::from_utf8(out).unwrap()
    }

    /// Build a minimal claude history event line.
    fn event(uuid: &str, parent: Option<&str>, kind: &str, body: Value) -> String {
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), Value::String(kind.into()));
        obj.insert("uuid".into(), Value::String(uuid.into()));
        obj.insert(
            "parentUuid".into(),
            parent
                .map(|p| Value::String(p.into()))
                .unwrap_or(Value::Null),
        );
        obj.insert("sessionId".into(), Value::String("test-session".into()));
        for (k, v) in body.as_object().cloned().unwrap_or_default() {
            obj.insert(k, v);
        }
        serde_json::to_string(&Value::Object(obj)).unwrap()
    }

    fn thinking_msg(text: &str, sig: &str) -> Value {
        serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": text, "signature": sig}
                ]
            }
        })
    }

    fn text_msg(text: &str) -> Value {
        serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}]
            }
        })
    }

    fn user_msg(text: &str) -> Value {
        serde_json::json!({
            "message": {
                "role": "user",
                "content": text
            }
        })
    }

    #[test]
    fn splice_thinking_only_between_user_and_text() {
        let input = [
            event("u1", None, "user", user_msg("hello")),
            event("a1", Some("u1"), "assistant", thinking_msg("hmm", "sig1")),
            event("a2", Some("a1"), "assistant", text_msg("hi back")),
        ]
        .join("\n");
        let out = sanitize_str(&input);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "thinking event should be dropped");
        // a2's parentUuid should now point to u1
        let a2: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(a2.get("parentUuid").and_then(Value::as_str), Some("u1"));
        // a2's uuid is preserved
        assert_eq!(a2.get("uuid").and_then(Value::as_str), Some("a2"));
        // The thinking signature did NOT survive — by virtue of the event being dropped.
        assert!(!out.contains("sig1"));
    }

    #[test]
    fn preserves_unknown_fields_on_kept_events() {
        // Synthesize a user event with a made-up extra field claude might add.
        let input = r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","newFutureField":"future-value","message":{"role":"user","content":"hi"}}"#;
        let out = sanitize_str(input);
        assert!(out.contains("newFutureField"));
        assert!(out.contains("future-value"));
    }

    #[test]
    fn user_event_is_untouched() {
        let input = event("u1", None, "user", user_msg("hello"));
        let out = sanitize_str(&input);
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v.get("type").and_then(Value::as_str), Some("user"));
        assert_eq!(v.get("uuid").and_then(Value::as_str), Some("u1"));
    }

    #[test]
    fn mixed_content_event_is_not_thinking_only() {
        // text + thinking together → not "thinking only" → kept untouched.
        let mixed = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                ]
            }
        });
        let input = event("a1", None, "assistant", mixed);
        let out = sanitize_str(&input);
        // Event was kept verbatim — signature still present.
        assert!(out.contains("sig"));
    }

    #[test]
    fn defensive_fallback_when_thinking_has_zero_children() {
        // Trailing thinking event (no child).
        let input = [
            event("u1", None, "user", user_msg("hello")),
            event("a1", Some("u1"), "assistant", thinking_msg("hmm", "sig1")),
        ]
        .join("\n");
        let out = sanitize_str(&input);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "fallback: keep event with blanked thinking");
        let a1: Value = serde_json::from_str(lines[1]).unwrap();
        let content = a1.pointer("/message/content/0").unwrap();
        assert_eq!(content.get("thinking").and_then(Value::as_str), Some(""));
        assert!(
            content.get("signature").is_none(),
            "signature should be removed"
        );
    }

    #[test]
    fn defensive_fallback_when_thinking_has_two_children() {
        // Two events both claiming the thinking event as parent.
        let input = [
            event("u1", None, "user", user_msg("hello")),
            event("a1", Some("u1"), "assistant", thinking_msg("hmm", "sig1")),
            event("a2", Some("a1"), "assistant", text_msg("first")),
            event("a3", Some("a1"), "assistant", text_msg("second")),
        ]
        .join("\n");
        let out = sanitize_str(&input);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "fallback path keeps all 4 events");
        let a1: Value = serde_json::from_str(lines[1]).unwrap();
        let content = a1.pointer("/message/content/0").unwrap();
        assert_eq!(content.get("thinking").and_then(Value::as_str), Some(""));
    }

    #[test]
    fn defensive_fallback_when_thinking_chained() {
        // Two consecutive thinking events (invariant violation).
        let input = [
            event("u1", None, "user", user_msg("hello")),
            event("a1", Some("u1"), "assistant", thinking_msg("first", "sig1")),
            event(
                "a2",
                Some("a1"),
                "assistant",
                thinking_msg("second", "sig2"),
            ),
            event("a3", Some("a2"), "assistant", text_msg("answer")),
        ]
        .join("\n");
        let out = sanitize_str(&input);
        let lines: Vec<&str> = out.lines().collect();
        // a1 has exactly 1 child (a2), so it WOULD have been splice-droppable...
        // BUT a2 is its child and a2 IS thinking-only, so a1 falls back to blanking.
        // a2's parent (a1) is thinking-only, so a2 also falls back to blanking.
        assert_eq!(
            lines.len(),
            4,
            "both thinking events kept (blanked) under invariant violation"
        );
    }

    #[test]
    fn parse_error_surfaces_loudly() {
        let input = "not valid json";
        let mut out = Vec::new();
        let err = strip_reasoning(Cursor::new(input), &mut out).unwrap_err();
        assert!(matches!(err, SanitizeError::Parse { .. }));
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert_eq!(sanitize_str(""), "");
        assert_eq!(sanitize_str("\n\n\n"), "");
    }

    #[test]
    fn sanitize_idempotent_after_first_pass() {
        let input = [
            event("u1", None, "user", user_msg("hello")),
            event("a1", Some("u1"), "assistant", thinking_msg("hmm", "sig1")),
            event("a2", Some("a1"), "assistant", text_msg("hi back")),
        ]
        .join("\n");
        let pass1 = sanitize_str(&input);
        let pass2 = sanitize_str(&pass1);
        assert_eq!(
            pass1, pass2,
            "applying sanitize twice yields the same result"
        );
    }
}
