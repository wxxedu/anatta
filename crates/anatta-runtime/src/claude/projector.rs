//! Projector implementations for the two claude raw formats.

use std::collections::HashMap;

use anatta_core::{
    AgentEvent, AgentEventEnvelope, AgentEventPayload, Backend, ProjectionContext, Projector,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::history;
use super::stream;

// ────────────────────────────────────────────────────────────────────────────
// HistoryProjector — disk session JSONL
// ────────────────────────────────────────────────────────────────────────────

/// Stateless projector for `<CLAUDE_CONFIG_DIR>/projects/.../<sess>.jsonl`
/// events. Emits only finalized variants — disk format never streams.
#[derive(Debug, Default, Clone, Copy)]
pub struct HistoryProjector;

impl HistoryProjector {
    pub fn new() -> Self {
        Self
    }
}

impl Projector for HistoryProjector {
    type Raw = history::ClaudeEvent;

    fn project(&mut self, ev: &Self::Raw, ctx: &ProjectionContext) -> Vec<AgentEvent> {
        match ev {
            history::ClaudeEvent::Assistant(a) => assistant(a, ctx),
            history::ClaudeEvent::User(u) => user(u, ctx),
            history::ClaudeEvent::System(s) => system(s, ctx),
            // queue-operation, attachment, permission-mode, ai-title,
            // last-prompt, custom-title, file-history-snapshot, pr-link,
            // worktree-state, progress: UI-internal, no semantic projection.
            _ => Vec::new(),
        }
    }
}

fn assistant(a: &history::AssistantEvent, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let env = AgentEventEnvelope {
        session_id: a.envelope.session_id.clone(),
        timestamp: parse_ts(&a.envelope.timestamp).unwrap_or(ctx.received_at),
        backend: Backend::Claude,
        raw_uuid: Some(a.envelope.uuid.clone()),
        parent_tool_use_id: None,
    };

    let mut out = Vec::new();
    for block in &a.message.content {
        let payload = match block {
            history::AssistantContentBlock::Thinking { thinking, .. } => {
                AgentEventPayload::Thinking { text: thinking.clone() }
            }
            history::AssistantContentBlock::Text { text } => {
                AgentEventPayload::AssistantText { text: text.clone() }
            }
            history::AssistantContentBlock::ToolUse { id, name, input, .. } => {
                AgentEventPayload::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }
            }
        };
        out.push(AgentEvent { envelope: env.clone(), payload });
    }

    out.push(AgentEvent {
        envelope: env,
        payload: AgentEventPayload::Usage {
            input_tokens: a.message.usage.input_tokens,
            output_tokens: a.message.usage.output_tokens,
            cache_read: a.message.usage.cache_read_input_tokens.unwrap_or(0),
            cache_create: a.message.usage.cache_creation_input_tokens.unwrap_or(0),
            cost_usd: None,
        },
    });

    out
}

fn user(u: &history::UserEvent, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let env = AgentEventEnvelope {
        session_id: u.envelope.session_id.clone(),
        timestamp: parse_ts(&u.envelope.timestamp).unwrap_or(ctx.received_at),
        backend: Backend::Claude,
        raw_uuid: Some(u.envelope.uuid.clone()),
        parent_tool_use_id: None,
    };

    match &u.message.content {
        history::UserMessageContent::Text(text) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::UserPrompt { text: text.clone() },
        }],
        history::UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                history::UserContentBlock::Text { text } => Some(AgentEvent {
                    envelope: env.clone(),
                    payload: AgentEventPayload::UserPrompt { text: text.clone() },
                }),
                history::UserContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let (text, structured) = unpack_tool_result(content);
                    Some(AgentEvent {
                        envelope: env.clone(),
                        payload: AgentEventPayload::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            success: !is_error.unwrap_or(false),
                            text,
                            structured,
                        },
                    })
                }
                history::UserContentBlock::Image { .. }
                | history::UserContentBlock::Document { .. } => None,
            })
            .collect(),
    }
}

fn unpack_tool_result(c: &history::ToolResultContent) -> (Option<String>, Option<Value>) {
    match c {
        history::ToolResultContent::Text(s) => (Some(s.clone()), None),
        history::ToolResultContent::Blocks(blocks) => {
            let mut text_buf = String::new();
            let mut has_text = false;
            for b in blocks {
                if let history::ToolResultBlock::Text { text } = b {
                    has_text = true;
                    text_buf.push_str(text);
                }
            }
            if has_text {
                (Some(text_buf), None)
            } else {
                (None, serde_json::to_value(blocks).ok())
            }
        }
    }
}

fn system(s: &history::SystemEvent, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let env = AgentEventEnvelope {
        session_id: s.envelope.session_id.clone(),
        timestamp: parse_ts(&s.envelope.timestamp).unwrap_or(ctx.received_at),
        backend: Backend::Claude,
        raw_uuid: Some(s.envelope.uuid.clone()),
        parent_tool_use_id: None,
    };
    match &s.kind {
        history::SystemKind::TurnDuration { .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::TurnCompleted {
                stop_reason: None,
                is_error: false,
            },
        }],
        history::SystemKind::ApiError { error, .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::Error {
                message: error.kind.clone().unwrap_or_else(|| "api_error".into()),
                fatal: false,
            },
        }],
        // stop_hook_summary / hook_progress / etc.: diagnostic, skip.
        _ => Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// StreamProjector — stdout `--output-format stream-json` (stateful)
// ────────────────────────────────────────────────────────────────────────────

/// Accumulates per-content-block state across claude's incremental SSE
/// stream so `*Delta` events carry snapshot semantics (`text_so_far`
/// is the full accumulated content, not a single chunk).
///
/// Allocate one per session.
#[derive(Debug, Default)]
pub struct StreamProjector {
    blocks: HashMap<u32, BlockAcc>,
}

#[derive(Debug)]
struct BlockAcc {
    kind: BlockAccKind,
}

#[derive(Debug)]
enum BlockAccKind {
    Text { text: String },
    Thinking { text: String },
    ToolUse { tool_use_id: String, partial_json: String },
    /// Block types we don't emit deltas for (redacted_thinking,
    /// container_upload, mcp_*, etc.). We still record the slot so
    /// content_block_stop won't underflow.
    Other,
}

impl StreamProjector {
    pub fn new() -> Self {
        Self::default()
    }

    fn project_stream_event(
        &mut self,
        s: &stream::PartialAssistantMessage,
        ctx: &ProjectionContext,
    ) -> Vec<AgentEvent> {
        use stream::BetaContentBlock as Cb;
        use stream::BetaRawContentBlockDelta as Dl;
        use stream::BetaRawMessageStreamEvent as Inner;

        let env = stream_envelope(
            s.session_id.clone(),
            Some(s.uuid.clone()),
            s.parent_tool_use_id.clone(),
            ctx,
        );

        match &s.event {
            Inner::ContentBlockStart { index, content_block } => {
                let kind = match content_block {
                    Cb::Text { .. } => BlockAccKind::Text { text: String::new() },
                    Cb::Thinking { .. } => BlockAccKind::Thinking { text: String::new() },
                    Cb::ToolUse { id, .. }
                    | Cb::ServerToolUse { id, .. }
                    | Cb::McpToolUse { id, .. } => BlockAccKind::ToolUse {
                        tool_use_id: id.clone(),
                        partial_json: String::new(),
                    },
                    _ => BlockAccKind::Other,
                };
                self.blocks.insert(*index, BlockAcc { kind });
                Vec::new()
            }
            Inner::ContentBlockDelta { index, delta } => {
                let Some(acc) = self.blocks.get_mut(index) else {
                    return Vec::new();
                };
                match (&mut acc.kind, delta) {
                    (BlockAccKind::Text { text }, Dl::TextDelta { text: chunk }) => {
                        text.push_str(chunk);
                        vec![AgentEvent {
                            envelope: env,
                            payload: AgentEventPayload::AssistantTextDelta {
                                content_block_index: *index,
                                text_so_far: text.clone(),
                            },
                        }]
                    }
                    (
                        BlockAccKind::Thinking { text },
                        Dl::ThinkingDelta { thinking: chunk },
                    ) => {
                        text.push_str(chunk);
                        vec![AgentEvent {
                            envelope: env,
                            payload: AgentEventPayload::ThinkingDelta {
                                content_block_index: *index,
                                text_so_far: text.clone(),
                            },
                        }]
                    }
                    (
                        BlockAccKind::ToolUse { tool_use_id, partial_json },
                        Dl::InputJsonDelta { partial_json: chunk },
                    ) => {
                        partial_json.push_str(chunk);
                        vec![AgentEvent {
                            envelope: env,
                            payload: AgentEventPayload::ToolUseInputDelta {
                                tool_use_id: tool_use_id.clone(),
                                content_block_index: *index,
                                partial_json: partial_json.clone(),
                            },
                        }]
                    }
                    // signature_delta, citations_delta, compaction_delta,
                    // mismatched pairs: not emitted as agent events.
                    _ => Vec::new(),
                }
            }
            Inner::ContentBlockStop { index } => {
                self.blocks.remove(index);
                Vec::new()
            }
            // message_start / message_delta / message_stop / ping / error:
            // higher-level lifecycle. Final content arrives via the parallel
            // `assistant` event and turn end via `result`, so skip here.
            _ => Vec::new(),
        }
    }
}

impl Projector for StreamProjector {
    type Raw = stream::ClaudeStreamEvent;

    fn project(&mut self, ev: &Self::Raw, ctx: &ProjectionContext) -> Vec<AgentEvent> {
        use stream::ClaudeStreamEvent::*;
        match ev {
            System(s) => stream_system(s, ctx),
            Assistant(a) => stream_assistant(a, ctx),
            User(u) => stream_user(u, ctx),
            Result(r) => stream_result(r, ctx),
            RateLimitEvent(r) => vec![AgentEvent {
                envelope: stream_envelope(
                    r.session_id.clone(),
                    Some(r.uuid.clone()),
                    None,
                    ctx,
                ),
                payload: AgentEventPayload::RateLimit {
                    limit_kind: r
                        .rate_limit_info
                        .rate_limit_type
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    // Claude reports a 0.0–1.0 fraction; we normalize to
                    // 0–100 for parity with codex's `used_percent`.
                    used_percent: r.rate_limit_info.utilization.map(|u| u * 100.0),
                    status: Some(rate_limit_status_str(&r.rate_limit_info.status).to_owned()),
                    resets_at: r.rate_limit_info.resets_at.map(|x| x as i64),
                    using_overage: r.rate_limit_info.is_using_overage,
                },
            }],
            StreamEvent(s) => self.project_stream_event(s, ctx),
            // Hooks / control plane / progress indicators / keep-alive: skip.
            ToolProgress(_) | ToolUseSummary(_) | AuthStatus(_)
            | PromptSuggestion(_) | KeepAlive | ControlRequest(_)
            | ControlResponse(_) | ControlCancelRequest(_)
            | PostTurnSummary(_) | TaskSummary(_) | TranscriptMirror(_) => Vec::new(),
        }
    }
}

fn stream_system(s: &stream::SystemMessage, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    use stream::SystemMessage::*;
    match s {
        Init(i) => vec![AgentEvent {
            envelope: stream_envelope(i.session_id.clone(), Some(i.uuid.clone()), None, ctx),
            payload: AgentEventPayload::SessionStarted {
                cwd: i.cwd.clone(),
                model: i.model.clone(),
                tools_available: i.tools.clone(),
            },
        }],
        ApiRetry(r) => vec![AgentEvent {
            envelope: stream_envelope(r.session_id.clone(), Some(r.uuid.clone()), None, ctx),
            payload: AgentEventPayload::Error {
                message: format!("api_retry attempt {} of {}", r.attempt, r.max_retries),
                fatal: false,
            },
        }],
        PermissionDenied(p) => vec![AgentEvent {
            envelope: stream_envelope(p.session_id.clone(), Some(p.uuid.clone()), None, ctx),
            payload: AgentEventPayload::Error {
                message: format!("permission_denied: {}", p.message),
                fatal: false,
            },
        }],
        _ => Vec::new(),
    }
}

fn stream_assistant(a: &stream::AssistantMessage, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let env = stream_envelope(
        a.session_id.clone(),
        Some(a.uuid.clone()),
        a.parent_tool_use_id.clone(),
        ctx,
    );

    let mut out = Vec::new();
    for block in &a.message.content {
        let payload = match block {
            stream::BetaContentBlock::Text { text, .. } => {
                AgentEventPayload::AssistantText { text: text.clone() }
            }
            stream::BetaContentBlock::Thinking { thinking, .. } => {
                AgentEventPayload::Thinking { text: thinking.clone() }
            }
            stream::BetaContentBlock::ToolUse { id, name, input, .. }
            | stream::BetaContentBlock::ServerToolUse { id, name, input, .. } => {
                AgentEventPayload::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }
            }
            stream::BetaContentBlock::McpToolUse { id, name, server_name, input } => {
                AgentEventPayload::ToolUse {
                    id: id.clone(),
                    name: format!("mcp/{server_name}/{name}"),
                    input: input.clone(),
                }
            }
            _ => continue,
        };
        out.push(AgentEvent { envelope: env.clone(), payload });
    }
    out
}

fn stream_user(u: &stream::UserMessage, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let session_id = u.session_id.clone().unwrap_or_else(|| ctx.session_id.clone());
    let env = stream_envelope(session_id, u.uuid.clone(), u.parent_tool_use_id.clone(), ctx);
    if let Some(text) = u.message.content.as_str() {
        return vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::UserPrompt { text: text.to_owned() },
        }];
    }
    if let Some(arr) = u.message.content.as_array() {
        let mut out = Vec::new();
        for item in arr {
            let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "text" => {
                    if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                        out.push(AgentEvent {
                            envelope: env.clone(),
                            payload: AgentEventPayload::UserPrompt { text: t.to_owned() },
                        });
                    }
                }
                "tool_result" => {
                    let tool_use_id = item
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let is_error = item.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    let (text, structured) =
                        if let Some(s) = item.get("content").and_then(|v| v.as_str()) {
                            (Some(s.to_owned()), None)
                        } else {
                            (None, item.get("content").cloned())
                        };
                    out.push(AgentEvent {
                        envelope: env.clone(),
                        payload: AgentEventPayload::ToolResult {
                            tool_use_id,
                            success: !is_error,
                            text,
                            structured,
                        },
                    });
                }
                _ => {}
            }
        }
        return out;
    }
    Vec::new()
}

fn stream_result(r: &stream::ResultMessage, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let (sid, uuid, stop_reason, is_error, usage_full, total_cost_usd) = match r {
        stream::ResultMessage::Success(s) => (
            s.session_id.clone(),
            s.uuid.clone(),
            s.stop_reason.clone(),
            s.is_error,
            Some(&s.usage),
            Some(s.total_cost_usd),
        ),
        stream::ResultMessage::ErrorDuringExecution(e)
        | stream::ResultMessage::ErrorMaxTurns(e)
        | stream::ResultMessage::ErrorMaxBudgetUsd(e)
        | stream::ResultMessage::ErrorMaxStructuredOutputRetries(e) => (
            e.session_id.clone(),
            e.uuid.clone(),
            e.stop_reason.clone(),
            e.is_error,
            Some(&e.usage),
            Some(e.total_cost_usd),
        ),
    };
    let env = stream_envelope(sid, Some(uuid), None, ctx);
    let mut out = Vec::with_capacity(2);
    if let Some(u) = usage_full {
        out.push(AgentEvent {
            envelope: env.clone(),
            payload: AgentEventPayload::Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read: u.cache_read_input_tokens,
                cache_create: u.cache_creation_input_tokens,
                cost_usd: total_cost_usd,
            },
        });
    }
    out.push(AgentEvent {
        envelope: env,
        payload: AgentEventPayload::TurnCompleted { stop_reason, is_error },
    });
    out
}

fn stream_envelope(
    session_id: String,
    raw_uuid: Option<String>,
    parent_tool_use_id: Option<String>,
    ctx: &ProjectionContext,
) -> AgentEventEnvelope {
    AgentEventEnvelope {
        session_id,
        timestamp: ctx.received_at,
        backend: Backend::Claude,
        raw_uuid,
        parent_tool_use_id,
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn rate_limit_status_str(s: &stream::RateLimitStatus) -> &'static str {
    use stream::RateLimitStatus::*;
    match s {
        Allowed => "ok",
        AllowedWarning => "warning",
        Rejected => "rejected",
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ProjectionContext {
        ProjectionContext {
            session_id: "test-session".into(),
            received_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn history_assistant_decomposes_into_thinking_text_tooluse_usage() {
        let line = r#"{"type":"assistant","uuid":"u","parentUuid":null,"sessionId":"s","timestamp":"2026-05-10T12:00:00Z","cwd":"/x","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2.1","message":{"id":"m","model":"opus","role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"sig"},{"type":"text","text":"hello"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let ev: history::ClaudeEvent = serde_json::from_str(line).unwrap();
        let mut p = HistoryProjector::new();
        let agent_events = p.project(&ev, &ctx());
        assert_eq!(agent_events.len(), 4);
        assert!(matches!(agent_events[0].payload, AgentEventPayload::Thinking { .. }));
        assert!(matches!(agent_events[1].payload, AgentEventPayload::AssistantText { .. }));
        assert!(matches!(agent_events[2].payload, AgentEventPayload::ToolUse { .. }));
        assert!(matches!(agent_events[3].payload, AgentEventPayload::Usage { .. }));
    }

    #[test]
    fn history_user_text() {
        let line = r#"{"type":"user","uuid":"u","parentUuid":null,"sessionId":"s","timestamp":"2026-05-10T12:00:00Z","cwd":"/x","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2.1","message":{"role":"user","content":"hi"}}"#;
        let ev: history::ClaudeEvent = serde_json::from_str(line).unwrap();
        let evs = HistoryProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 1);
        match &evs[0].payload {
            AgentEventPayload::UserPrompt { text } => assert_eq!(text, "hi"),
            _ => panic!("expected UserPrompt"),
        }
    }

    #[test]
    fn history_user_tool_result() {
        let line = r#"{"type":"user","uuid":"u","parentUuid":null,"sessionId":"s","timestamp":"2026-05-10T12:00:00Z","cwd":"/x","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2.1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"output"}]}}"#;
        let ev: history::ClaudeEvent = serde_json::from_str(line).unwrap();
        let evs = HistoryProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 1);
        match &evs[0].payload {
            AgentEventPayload::ToolResult { tool_use_id, success, text, .. } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert!(*success);
                assert_eq!(text.as_deref(), Some("output"));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn stream_init_to_session_started() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/x","session_id":"s","tools":["Bash","Edit"],"mcp_servers":[],"model":"opus","permissionMode":"default","slash_commands":[],"apiKeySource":"none","claude_code_version":"2.1","output_style":"default","skills":[],"plugins":[],"uuid":"u"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(line).unwrap();
        let mut p = StreamProjector::new();
        let evs = p.project(&ev, &ctx());
        assert_eq!(evs.len(), 1);
        match &evs[0].payload {
            AgentEventPayload::SessionStarted { cwd, model, tools_available } => {
                assert_eq!(cwd, "/x");
                assert_eq!(model, "opus");
                assert_eq!(tools_available.len(), 2);
            }
            _ => panic!("expected SessionStarted"),
        }
    }

    #[test]
    fn stream_rate_limit_event() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1778414400,"rateLimitType":"five_hour","utilization":0.87,"isUsingOverage":false},"uuid":"u","session_id":"s"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(line).unwrap();
        let evs = StreamProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 1);
        match &evs[0].payload {
            AgentEventPayload::RateLimit {
                limit_kind,
                used_percent,
                status,
                resets_at,
                using_overage,
            } => {
                assert_eq!(limit_kind, "five_hour");
                assert_eq!(resets_at, &Some(1778414400));
                assert_eq!(using_overage, &Some(false));
                // 0.87 → 87.0 (0–100 normalized)
                assert!(
                    (used_percent.unwrap() - 87.0).abs() < 1e-9,
                    "got {used_percent:?}"
                );
                assert_eq!(status.as_deref(), Some("warning"));
            }
            _ => panic!("expected RateLimit"),
        }
    }

    #[test]
    fn stream_result_emits_usage_then_turn_completed() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":100,"duration_api_ms":80,"num_turns":1,"result":"ok","stop_reason":"end_turn","session_id":"s","total_cost_usd":0.001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":""},"modelUsage":{},"permission_denials":[],"uuid":"u"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(line).unwrap();
        let evs = StreamProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0].payload, AgentEventPayload::Usage { .. }));
        assert!(matches!(evs[1].payload, AgentEventPayload::TurnCompleted { .. }));
    }

    #[test]
    fn stream_projector_accumulates_text_deltas() {
        let mut p = StreamProjector::new();
        let c = ctx();

        let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"s","parent_tool_use_id":null,"uuid":"u1"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(start).unwrap();
        assert!(p.project(&ev, &c).is_empty());

        let d1 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}},"session_id":"s","parent_tool_use_id":null,"uuid":"u2"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(d1).unwrap();
        let evs = p.project(&ev, &c);
        match &evs[0].payload {
            AgentEventPayload::AssistantTextDelta { text_so_far, .. } => {
                assert_eq!(text_so_far, "Hel");
            }
            _ => panic!("expected AssistantTextDelta"),
        }

        let d2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}},"session_id":"s","parent_tool_use_id":null,"uuid":"u3"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(d2).unwrap();
        let evs = p.project(&ev, &c);
        match &evs[0].payload {
            AgentEventPayload::AssistantTextDelta { text_so_far, .. } => {
                assert_eq!(text_so_far, "Hello", "snapshot, not increment");
            }
            _ => panic!("expected AssistantTextDelta"),
        }

        let stop = r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"s","parent_tool_use_id":null,"uuid":"u4"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(stop).unwrap();
        assert!(p.project(&ev, &c).is_empty());
    }

    #[test]
    fn stream_projector_accumulates_input_json_deltas() {
        let mut p = StreamProjector::new();
        let c = ctx();

        let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}},"session_id":"s","parent_tool_use_id":null,"uuid":"u1"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(start).unwrap();
        let _ = p.project(&ev, &c);

        let d1 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}},"session_id":"s","parent_tool_use_id":null,"uuid":"u2"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(d1).unwrap();
        let evs = p.project(&ev, &c);
        match &evs[0].payload {
            AgentEventPayload::ToolUseInputDelta { tool_use_id, partial_json, .. } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert_eq!(partial_json, "{\"command\":");
            }
            _ => panic!("expected ToolUseInputDelta"),
        }

        let d2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" \"ls\"}"}},"session_id":"s","parent_tool_use_id":null,"uuid":"u3"}"#;
        let ev: stream::ClaudeStreamEvent = serde_json::from_str(d2).unwrap();
        let evs = p.project(&ev, &c);
        match &evs[0].payload {
            AgentEventPayload::ToolUseInputDelta { partial_json, .. } => {
                assert_eq!(partial_json, "{\"command\": \"ls\"}");
            }
            _ => panic!("expected ToolUseInputDelta"),
        }
    }
}
