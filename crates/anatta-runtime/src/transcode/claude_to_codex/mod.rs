//! Transcode claude history JSONL → codex rollout JSONL.
//!
//! Mapping (v1, sub-agent recursion deferred to a future tier):
//!
//! | Claude line                                  | Codex emission                         |
//! |----------------------------------------------|----------------------------------------|
//! | `system/init` (first line)                   | `session_meta` + `turn_context`        |
//! | `user` text content                          | `response_item::message{role:user}`    |
//! | `user` tool_result content                   | `response_item::function_call_output`  |
//! | `assistant` text content                     | `response_item::message{role:assistant}` |
//! | `assistant` thinking content                 | **drop**                               |
//! | `assistant` tool_use content                 | `response_item::function_call`         |
//! | `system/compact_boundary`                    | suppressed (the following user line emits `compacted`) |
//! | `user` with `isCompactSummary:true`          | `compacted{message}`                   |
//! | metadata fields (uuid, parentUuid, ...)      | dropped                                |
//!
//! Sub-agent (`Task`) tool calls in v1 are emitted as plain
//! `FunctionCall { name: "Task", arguments }` with a paired output if
//! the sub transcript is reachable from sidecar; otherwise paired with
//! a placeholder `"(sub-agent transcript unavailable)"`.
//!
//! Reasoning blocks (`thinking`) drop unconditionally per the design's
//! reasoning-cannot-cross-engines invariant.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::id_mint::{map_tool_call_id, view_session_id};
use super::{Engine, TranscodeError, TranscodeInput, TranscodeOutput};

pub(super) fn run(
    input: TranscodeInput<'_>,
    view_dir: &Path,
) -> Result<TranscodeOutput, TranscodeError> {
    let view_id = view_session_id(input.source_engine_session_id, Engine::Codex);

    // Atomic write: <view_dir>.tmp → <view_dir>
    let tmp_dir = with_tmp_suffix(view_dir);
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }
    fs::create_dir_all(&tmp_dir)?;

    let view_events_path = tmp_dir.join("rollout.jsonl");
    let view_sidecar_dir = tmp_dir.join("subagents");
    fs::create_dir_all(&view_sidecar_dir)?;

    let result = (|| -> Result<(), TranscodeError> {
        let src = fs::File::open(input.source_events_jsonl)?;
        let reader = BufReader::new(src);
        let out_file = fs::File::create(&view_events_path)?;
        let mut out = BufWriter::new(out_file);

        // Preamble
        write_codex_preamble(&mut out, &view_id, input.conversation_cwd)?;

        for (line_idx, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&line).map_err(|e| TranscodeError::Parse {
                line: line_idx,
                source: e,
            })?;
            transcode_one(&v, &mut out)?;
        }

        out.flush()?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }

    // Atomically promote
    let final_dir = view_dir;
    if final_dir.exists() {
        fs::remove_dir_all(final_dir)?;
    }
    fs::rename(&tmp_dir, final_dir)?;

    Ok(TranscodeOutput {
        view_engine_session_id: view_id,
        view_events_path: final_dir.join("rollout.jsonl"),
        view_sidecar_dir: final_dir.join("subagents"),
    })
}

fn write_codex_preamble<W: Write>(
    out: &mut W,
    view_id: &str,
    cwd: &str,
) -> Result<(), TranscodeError> {
    let now = chrono::Utc::now().to_rfc3339();
    let session_meta = json!({
        "type": "session_meta",
        "timestamp": &now,
        "payload": {
            "id": view_id,
            "cwd": cwd,
            "originator": "anatta-transcoder",
            "cli_version": "anatta-transcoder/v1",
            "timestamp": &now,
            "model_provider": "openai",
            "source": "exec",
        }
    });
    let turn_context = json!({
        "type": "turn_context",
        "timestamp": &now,
        "payload": {
            "cwd": cwd,
            "model": "",
            "approval_policy": "never",
            "sandbox_policy": { "type": "danger_full_access" },
        }
    });
    writeln!(out, "{}", session_meta)?;
    writeln!(out, "{}", turn_context)?;
    Ok(())
}

fn transcode_one<W: Write>(v: &Value, out: &mut W) -> Result<(), TranscodeError> {
    let event_type = v.get("type").and_then(Value::as_str).unwrap_or("");

    match event_type {
        "user" => transcode_user(v, out)?,
        "assistant" => transcode_assistant(v, out)?,
        "system" => transcode_system(v, out)?,
        // First-line system/init is also "system" with subtype 'init' on
        // newer wire; falls into the `system` branch above.
        // 'attachment' root events are degraded to a user text line below.
        "attachment" => transcode_attachment(v, out)?,
        // Unknown event types are dropped (rather than failing) so the
        // transcoder is resilient to claude wire-format additions.
        _ => {}
    }
    Ok(())
}

fn transcode_user<W: Write>(v: &Value, out: &mut W) -> Result<(), TranscodeError> {
    // Detect isCompactSummary first — that's the cross-engine equivalent
    // of codex's `compacted` line.
    let is_compact_summary = v
        .get("isCompactSummary")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let content = v.get("message").and_then(|m| m.get("content"));

    if is_compact_summary {
        let summary_text = extract_first_text(content).unwrap_or_default();
        let now = chrono::Utc::now().to_rfc3339();
        let compacted = json!({
            "type": "compacted",
            "timestamp": now,
            "payload": {
                "message": summary_text,
            }
        });
        writeln!(out, "{}", compacted)?;
        return Ok(());
    }

    // Walk message.content[] and emit per-block.
    let Some(content) = content else {
        return Ok(());
    };
    if let Some(arr) = content.as_array() {
        for block in arr {
            let bt = block.get("type").and_then(Value::as_str).unwrap_or("");
            match bt {
                "text" => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    emit_codex_message(out, "user", text)?;
                }
                "image" => {
                    // Best-effort: emit an input_image with the raw fields.
                    let now = chrono::Utc::now().to_rfc3339();
                    let item = json!({
                        "type": "response_item",
                        "timestamp": now,
                        "payload": {
                            "type": "message",
                            "role": "user",
                            "content": [{
                                "type": "input_image",
                                // Pass through the rest of the image block verbatim.
                                "source": block.get("source").cloned().unwrap_or(Value::Null),
                            }],
                        }
                    });
                    writeln!(out, "{}", item)?;
                }
                "tool_result" => {
                    let tool_use_id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    let mapped = map_tool_call_id(tool_use_id, Engine::Claude, Engine::Codex);
                    let output = stringify_tool_result_content(block.get("content"));
                    let now = chrono::Utc::now().to_rfc3339();
                    let item = json!({
                        "type": "response_item",
                        "timestamp": now,
                        "payload": {
                            "type": "function_call_output",
                            "call_id": mapped,
                            "output": output,
                        }
                    });
                    writeln!(out, "{}", item)?;
                }
                _ => {} // unknown block type: drop
            }
        }
    } else if let Some(text) = content.as_str() {
        // Older claude schemas sometimes use string content; emit as text.
        emit_codex_message(out, "user", text)?;
    }
    Ok(())
}

fn transcode_assistant<W: Write>(v: &Value, out: &mut W) -> Result<(), TranscodeError> {
    let content = v.get("message").and_then(|m| m.get("content"));
    let Some(content) = content else {
        return Ok(());
    };
    let Some(arr) = content.as_array() else {
        if let Some(text) = content.as_str() {
            emit_codex_message(out, "assistant", text)?;
        }
        return Ok(());
    };
    for block in arr {
        let bt = block.get("type").and_then(Value::as_str).unwrap_or("");
        match bt {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                emit_codex_message(out, "assistant", text)?;
            }
            "thinking" => {
                // Drop unconditionally — see crate docs.
            }
            "tool_use" => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                let mapped = map_tool_call_id(id, Engine::Claude, Engine::Codex);
                let args_str = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_owned());
                let now = chrono::Utc::now().to_rfc3339();
                let item = json!({
                    "type": "response_item",
                    "timestamp": now,
                    "payload": {
                        "type": "function_call",
                        "call_id": mapped,
                        "name": name,
                        "arguments": args_str,
                    }
                });
                writeln!(out, "{}", item)?;
            }
            _ => {} // unknown: drop
        }
    }
    Ok(())
}

fn transcode_system<W: Write>(v: &Value, _out: &mut W) -> Result<(), TranscodeError> {
    // `system/init` is consumed by our preamble (we write our own).
    // `system/compact_boundary` is consumed alongside the next isCompactSummary user.
    // Other system subtypes are dropped — codex has no equivalent surface.
    let _ = v;
    Ok(())
}

fn transcode_attachment<W: Write>(v: &Value, out: &mut W) -> Result<(), TranscodeError> {
    // Degrade attachments to a user-visible text line. Best-effort.
    let path = v.get("path").and_then(Value::as_str).unwrap_or("");
    let text = format!("[attachment: {}]", path);
    emit_codex_message(out, "user", &text)
}

fn emit_codex_message<W: Write>(
    out: &mut W,
    role: &str,
    text: &str,
) -> Result<(), TranscodeError> {
    let content_type = if role == "assistant" { "output_text" } else { "input_text" };
    let now = chrono::Utc::now().to_rfc3339();
    let item = json!({
        "type": "response_item",
        "timestamp": now,
        "payload": {
            "type": "message",
            "role": role,
            "content": [{ "type": content_type, "text": text }],
        }
    });
    writeln!(out, "{}", item)?;
    Ok(())
}

fn extract_first_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(s) = content.as_str() {
        return Some(s.to_owned());
    }
    let arr = content.as_array()?;
    for block in arr {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                return Some(t.to_owned());
            }
        }
    }
    None
}

fn stringify_tool_result_content(c: Option<&Value>) -> Value {
    let Some(c) = c else { return Value::String(String::new()) };
    // Claude tool_result.content can be a string OR an array of typed blocks.
    if let Some(s) = c.as_str() {
        return Value::String(s.to_owned());
    }
    if let Some(arr) = c.as_array() {
        // Concatenate text blocks into a single string; pass through other shapes verbatim.
        let mut combined = String::new();
        for block in arr {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    combined.push_str(t);
                }
            } else {
                // Non-text block — fall back to serializing the entire array.
                return c.clone();
            }
        }
        return Value::String(combined);
    }
    c.clone()
}

fn with_tmp_suffix(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests;
