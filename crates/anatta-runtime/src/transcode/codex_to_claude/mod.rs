//! Transcode codex rollout JSONL → claude history JSONL.
//!
//! Mapping (v1):
//!
//! | Codex line                                  | Claude emission                           |
//! |---------------------------------------------|-------------------------------------------|
//! | first `session_meta` (bootstrap)            | `system/init` (sessionId = view id, cwd)  |
//! | first `turn_context` (bootstrap)            | dropped — folded into `system/init` model |
//! | subsequent `turn_context` (mid-session)     | dropped (no claude analogue)              |
//! | `response_item::message{role:user, InputText}`     | `user` line, `message.content[]{type:"text"}` |
//! | `response_item::message{role:user, InputImage}`    | `user` line, `message.content[]{type:"image"}` |
//! | `response_item::message{role:assistant, OutputText}` | `assistant` line, content `type:"text"`  |
//! | `response_item::message{role:developer, ...}`      | dropped (claude has no developer role at this surface) |
//! | `response_item::reasoning`                  | **drop**                                  |
//! | `response_item::function_call`              | `assistant` `tool_use{name, input}`       |
//! | `response_item::function_call_output`       | `user` `tool_result{tool_use_id, content}` |
//! | `response_item::custom_tool_call`/`_output` | same shape as function_call/output        |
//! | `response_item::web_search_call`            | `assistant` `tool_use{name:"WebSearch"}`  |
//! | `response_item::ghost_snapshot`             | dropped                                   |
//! | `event_msg::agent_message` / `user_message` | dropped (already emitted as response_item) |
//! | `event_msg::compacted` / `context_compacted`| `system/compact_boundary` + synthetic `isCompactSummary` user |
//! | `event_msg::exec_command_end` etc.          | dropped (paired function_call_output already carries the result) |
//! | other `event_msg`s                          | dropped                                   |
//!
//! Claude DAG synthesis: each emitted line gets a deterministic uuid
//! (`synth_claude_uuid`) and `parentUuid` is the previous emitted uuid
//! (linear chain). First line `parentUuid = null`.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::id_mint::{map_tool_call_id, synth_claude_uuid, view_session_id};
use super::{Engine, TranscodeError, TranscodeInput, TranscodeOutput};

pub(super) fn run(
    input: TranscodeInput<'_>,
    view_dir: &Path,
) -> Result<TranscodeOutput, TranscodeError> {
    let view_id = view_session_id(input.source_engine_session_id, Engine::Claude);

    let tmp_dir = with_tmp_suffix(view_dir);
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }
    fs::create_dir_all(&tmp_dir)?;

    let view_events_path = tmp_dir.join("events.jsonl");
    let view_sidecar_dir = tmp_dir.join("sidecar");
    fs::create_dir_all(&view_sidecar_dir)?;

    let result = (|| -> Result<(), TranscodeError> {
        let src = fs::File::open(input.source_events_jsonl)?;
        let reader = BufReader::new(src);

        let out_file = fs::File::create(&view_events_path)?;
        let mut out = BufWriter::new(out_file);

        let mut state = EmitState {
            view_id: view_id.clone(),
            cwd: input.conversation_cwd.to_owned(),
            emit_index: 0,
            prev_uuid: None,
            seen_first_session_meta: false,
            seen_first_turn_context: false,
            session_init_emitted: false,
            model: String::new(),
        };

        for (line_idx, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&line).map_err(|e| TranscodeError::Parse {
                line: line_idx,
                source: e,
            })?;
            transcode_one(&v, &mut state, &mut out)?;
        }

        out.flush()?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(e);
    }

    let final_dir = view_dir;
    if final_dir.exists() {
        fs::remove_dir_all(final_dir)?;
    }
    fs::rename(&tmp_dir, final_dir)?;

    Ok(TranscodeOutput {
        view_engine_session_id: view_id,
        view_events_path: final_dir.join("events.jsonl"),
        view_sidecar_dir: final_dir.join("sidecar"),
    })
}

struct EmitState {
    view_id: String,
    cwd: String,
    emit_index: usize,
    prev_uuid: Option<String>,
    seen_first_session_meta: bool,
    seen_first_turn_context: bool,
    session_init_emitted: bool,
    model: String,
}

impl EmitState {
    fn next_uuid(&mut self) -> String {
        let u = synth_claude_uuid(&self.view_id, self.emit_index);
        self.emit_index += 1;
        u
    }

    fn emit<W: Write>(&mut self, mut obj: Value, out: &mut W) -> Result<(), TranscodeError> {
        let uuid = self.next_uuid();
        let parent = self.prev_uuid.clone();
        let m = obj.as_object_mut().expect("emit() requires JSON object");
        m.insert("uuid".to_owned(), Value::String(uuid.clone()));
        m.insert(
            "parentUuid".to_owned(),
            parent.map(Value::String).unwrap_or(Value::Null),
        );
        m.insert("sessionId".to_owned(), Value::String(self.view_id.clone()));
        m.insert("cwd".to_owned(), Value::String(self.cwd.clone()));
        m.insert("timestamp".to_owned(), Value::String(chrono::Utc::now().to_rfc3339()));
        writeln!(out, "{}", obj)?;
        self.prev_uuid = Some(uuid);
        Ok(())
    }
}

fn transcode_one<W: Write>(
    v: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let event_type = v.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = v.get("payload").unwrap_or(&Value::Null);

    match event_type {
        "session_meta" => {
            if !state.seen_first_session_meta {
                state.seen_first_session_meta = true;
                // Defer system/init emission until we also have a turn_context
                // (so we can include model). If turn_context never arrives,
                // emit system/init on first response_item.
            }
            // Subsequent session_metas (shouldn't happen) dropped.
        }
        "turn_context" => {
            if !state.seen_first_turn_context {
                state.seen_first_turn_context = true;
                state.model = payload
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
            }
            // Mid-session turn_contexts dropped.
        }
        "response_item" => {
            ensure_session_init(state, out)?;
            transcode_response_item(payload, state, out)?;
        }
        "event_msg" => {
            ensure_session_init(state, out)?;
            transcode_event_msg(payload, state, out)?;
        }
        "compacted" => {
            ensure_session_init(state, out)?;
            emit_compact_boundary_and_summary(payload, state, out)?;
        }
        // unknown event types drop silently
        _ => {}
    }
    Ok(())
}

fn ensure_session_init<W: Write>(
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    if state.session_init_emitted {
        return Ok(());
    }
    state.session_init_emitted = true;
    let obj = json!({
        "type": "system",
        "subtype": "init",
        "model": state.model,
        "entrypoint": "anatta-transcoder",
        "version": "anatta-transcoder/v1",
        "slug": "anatta-view",
    });
    state.emit(obj, out)
}

fn transcode_response_item<W: Write>(
    payload: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "message" => transcode_message(payload, state, out),
        "reasoning" => Ok(()), // drop
        "function_call" => transcode_function_call(payload, state, out),
        "function_call_output" => transcode_function_call_output(payload, state, out),
        "custom_tool_call" => transcode_function_call(payload, state, out),
        "custom_tool_call_output" => transcode_function_call_output(payload, state, out),
        "web_search_call" => transcode_web_search_call(payload, state, out),
        "ghost_snapshot" => Ok(()), // drop
        _ => Ok(()),                 // unknown: drop
    }
}

fn transcode_message<W: Write>(
    payload: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
    if role == "developer" {
        // Claude has no native developer role at this surface; drop.
        // (Codex injects developer messages as bootstrap policy/instructions.)
        return Ok(());
    }
    let claude_role = match role {
        "user" => "user",
        "assistant" => "assistant",
        _ => return Ok(()),
    };

    let content_arr = payload
        .get("content")
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let mut claude_blocks: Vec<Value> = Vec::new();
    for block in content_arr {
        let bt = block.get("type").and_then(Value::as_str).unwrap_or("");
        match bt {
            "input_text" | "output_text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                claude_blocks.push(json!({"type":"text","text": text}));
            }
            "input_image" => {
                // Pass image source verbatim; claude's image block accepts a
                // flexible `source` shape.
                let source = block.get("source").cloned().unwrap_or(Value::Null);
                claude_blocks.push(json!({"type":"image","source": source}));
            }
            _ => {} // unknown content block: drop
        }
    }
    if claude_blocks.is_empty() {
        return Ok(());
    }
    let event_type = if claude_role == "user" { "user" } else { "assistant" };
    let obj = json!({
        "type": event_type,
        "message": {
            "role": claude_role,
            "content": claude_blocks,
        }
    });
    state.emit(obj, out)
}

fn transcode_function_call<W: Write>(
    payload: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
    let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
    let args_str = payload.get("arguments").and_then(Value::as_str).unwrap_or("{}");
    let input: Value = serde_json::from_str(args_str).unwrap_or(Value::Object(Default::default()));
    let mapped = map_tool_call_id(call_id, Engine::Codex, Engine::Claude);
    let obj = json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": mapped,
                "name": name,
                "input": input,
            }],
        }
    });
    state.emit(obj, out)
}

fn transcode_function_call_output<W: Write>(
    payload: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let call_id = payload.get("call_id").and_then(Value::as_str).unwrap_or("");
    let mapped = map_tool_call_id(call_id, Engine::Codex, Engine::Claude);
    // codex output may be a string or arbitrary value; claude tool_result.content
    // accepts string or block array. Coerce to a single text block for simplicity.
    let raw_output = payload.get("output").cloned().unwrap_or(Value::Null);
    let content_str = match &raw_output {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    let obj = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": mapped,
                "content": content_str,
            }],
        }
    });
    state.emit(obj, out)
}

fn transcode_web_search_call<W: Write>(
    payload: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let synth_id = format!("anatta-cx-websearch-{}", state.emit_index);
    let action = payload.get("action").cloned().unwrap_or(Value::Null);
    let tool_use = json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": synth_id.clone(),
                "name": "WebSearch",
                "input": action,
            }],
        }
    });
    state.emit(tool_use, out)?;
    // Paired empty result so the assistant turn has a closing tool_result.
    // codex normally emits WebSearchEnd as event_msg, which we drop; this
    // ensures claude's DAG remains well-formed.
    let result = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": synth_id,
                "content": "",
            }],
        }
    });
    state.emit(result, out)
}

fn transcode_event_msg<W: Write>(
    payload: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "context_compacted" => emit_compact_boundary_and_summary(
            &json!({"message": ""}),
            state,
            out,
        ),
        // The rest are duplicates of response_item content or codex-internal
        // UI signals; drop.
        _ => Ok(()),
    }
}

fn emit_compact_boundary_and_summary<W: Write>(
    payload: &Value,
    state: &mut EmitState,
    out: &mut W,
) -> Result<(), TranscodeError> {
    let summary = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    // The compact_boundary itself.
    let boundary = json!({
        "type": "system",
        "subtype": "compact_boundary",
        "compactMetadata": {
            "trigger": "manual",
        }
    });
    // Note: compact_boundary in claude has parentUuid:null + logicalParentUuid
    // pointing back. Our emit() always sets parentUuid from prev; for v1 we
    // accept the deviation (still parses; downstream just sees a new root).
    state.emit(boundary, out)?;
    let synth_user = json!({
        "type": "user",
        "isCompactSummary": true,
        "message": {
            "role": "user",
            "content": [{"type":"text","text": summary}],
        }
    });
    state.emit(synth_user, out)
}

fn with_tmp_suffix(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests;
