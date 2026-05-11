//! Projector implementations for the two codex raw formats.

use anatta_core::{
    AgentEvent, AgentEventEnvelope, AgentEventPayload, Backend, ProjectionContext, Projector,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::history;
use super::stream;

// ────────────────────────────────────────────────────────────────────────────
// HistoryProjector — `$CODEX_HOME/sessions/.../rollout-*.jsonl`
// ────────────────────────────────────────────────────────────────────────────

/// Stateless projector for codex rollout events.
#[derive(Debug, Default, Clone, Copy)]
pub struct HistoryProjector;

impl HistoryProjector {
    pub fn new() -> Self {
        Self
    }
}

impl Projector for HistoryProjector {
    type Raw = history::CodexEvent;

    fn project(&mut self, ev: &Self::Raw, ctx: &ProjectionContext) -> Vec<AgentEvent> {
        let ts = parse_ts(&ev.timestamp).unwrap_or(ctx.received_at);
        use history::CodexEventKind::*;
        match &ev.kind {
            SessionMeta(m) => vec![AgentEvent {
                envelope: codex_envelope(m.id.clone(), None, ts),
                payload: AgentEventPayload::SessionStarted {
                    cwd: m.cwd.clone(),
                    model: String::new(),
                    tools_available: Vec::new(),
                },
            }],
            TurnContext(_) => vec![AgentEvent {
                envelope: codex_envelope(ctx.session_id.clone(), None, ts),
                payload: AgentEventPayload::TurnStarted,
            }],
            ResponseItem(item) => response_item(item, ts, ctx),
            EventMsg(msg) => event_msg(msg, ts, ctx),
            // Compacted: diagnostic only.
            Compacted(_) => Vec::new(),
        }
    }
}

fn response_item(
    item: &history::ResponseItem,
    ts: DateTime<Utc>,
    ctx: &ProjectionContext,
) -> Vec<AgentEvent> {
    let env = codex_envelope(ctx.session_id.clone(), None, ts);
    use history::ResponseItem::*;
    match item {
        Message { role, content, .. } => content
            .iter()
            .filter_map(|c| match c {
                history::MessageContent::OutputText { text } if role == "assistant" => {
                    Some(AgentEvent {
                        envelope: env.clone(),
                        payload: AgentEventPayload::AssistantText { text: text.clone() },
                    })
                }
                history::MessageContent::InputText { text } if role == "user" => {
                    Some(AgentEvent {
                        envelope: env.clone(),
                        payload: AgentEventPayload::UserPrompt { text: text.clone() },
                    })
                }
                _ => None,
            })
            .collect(),
        Reasoning { content, .. } => {
            let mut buf = String::new();
            if let Some(arr) = content.as_array() {
                for b in arr {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        buf.push_str(t);
                    }
                }
            }
            if buf.is_empty() {
                Vec::new()
            } else {
                vec![AgentEvent {
                    envelope: env,
                    payload: AgentEventPayload::Thinking { text: buf },
                }]
            }
        }
        FunctionCall { call_id, name, arguments, .. } => {
            // arguments is JSON-encoded as a string (OpenAI tool-call convention).
            let input: Value = serde_json::from_str(arguments)
                .unwrap_or_else(|_| Value::String(arguments.clone()));
            vec![AgentEvent {
                envelope: env,
                payload: AgentEventPayload::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input,
                },
            }]
        }
        FunctionCallOutput { call_id, output } => {
            let (text, structured) = unpack_output(output);
            vec![AgentEvent {
                envelope: env,
                payload: AgentEventPayload::ToolResult {
                    tool_use_id: call_id.clone(),
                    success: true,
                    text,
                    structured,
                },
            }]
        }
        CustomToolCall { call_id, name, input, .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::ToolUse {
                id: call_id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
        }],
        CustomToolCallOutput { call_id, output } => {
            let (text, structured) = unpack_output(output);
            vec![AgentEvent {
                envelope: env,
                payload: AgentEventPayload::ToolResult {
                    tool_use_id: call_id.clone(),
                    success: true,
                    text,
                    structured,
                },
            }]
        }
        WebSearchCall { action, .. } => {
            let query = action.get("query").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            vec![AgentEvent {
                envelope: env,
                payload: AgentEventPayload::ToolUse {
                    id: String::new(),
                    name: "web_search".into(),
                    input: serde_json::json!({ "query": query }),
                },
            }]
        }
        GhostSnapshot { .. } => Vec::new(),
    }
}

fn unpack_output(output: &Value) -> (Option<String>, Option<Value>) {
    if let Some(s) = output.as_str() {
        return (Some(s.to_owned()), None);
    }
    if let Some(s) = output.get("output").and_then(|v| v.as_str()) {
        return (Some(s.to_owned()), Some(output.clone()));
    }
    (None, Some(output.clone()))
}

fn event_msg(msg: &history::EventMsg, ts: DateTime<Utc>, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let env = codex_envelope(ctx.session_id.clone(), None, ts);
    use history::EventMsg::*;
    match msg {
        AgentMessage { message, .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::AssistantText { text: message.clone() },
        }],
        AgentReasoning { text } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::Thinking { text: text.clone() },
        }],
        UserMessage { message, .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::UserPrompt { text: message.clone() },
        }],
        TaskStarted { .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::TurnStarted,
        }],
        TaskComplete { .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::TurnCompleted {
                stop_reason: None,
                is_error: false,
            },
        }],
        TurnAborted { reason, .. } => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::TurnCompleted {
                stop_reason: Some(reason.clone()),
                is_error: true,
            },
        }],
        TokenCount { info, rate_limits } => {
            let input = info.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let output = info.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_read = info.get("cached_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let mut out = vec![AgentEvent {
                envelope: env.clone(),
                payload: AgentEventPayload::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read,
                    cache_create: 0,
                    cost_usd: None,
                },
            }];
            if let Some(snap) = rate_limits {
                for ev in rate_limit_events_from_snapshot(&env, snap) {
                    out.push(ev);
                }
            }
            out
        }
        // ExecCommandEnd / PatchApplyEnd / WebSearchEnd / McpToolCallEnd /
        // ViewImageToolCall / CollabAgent* / GuardianAssessment /
        // ThreadNameUpdated / ContextCompacted: diagnostic post-hocs over
        // response_item-emitted ToolUse events, skip.
        _ => Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// StreamProjector — `codex exec --json`
// ────────────────────────────────────────────────────────────────────────────

/// Stateless projector for codex stdout streaming events.
///
/// Codex emits full snapshots in `item.updated`, so no accumulator is
/// needed — `*Delta` events can be derived from each event in
/// isolation.
#[derive(Debug, Default, Clone, Copy)]
pub struct StreamProjector;

impl StreamProjector {
    pub fn new() -> Self {
        Self
    }
}

impl Projector for StreamProjector {
    type Raw = stream::CodexStreamEvent;

    fn project(&mut self, ev: &Self::Raw, ctx: &ProjectionContext) -> Vec<AgentEvent> {
        use stream::CodexStreamEvent::*;
        match ev {
            ThreadStarted(t) => vec![AgentEvent {
                envelope: codex_envelope(t.thread_id.clone(), None, ctx.received_at),
                payload: AgentEventPayload::SessionStarted {
                    cwd: String::new(),
                    model: String::new(),
                    tools_available: Vec::new(),
                },
            }],
            TurnStarted(_) => vec![AgentEvent {
                envelope: codex_envelope(ctx.session_id.clone(), None, ctx.received_at),
                payload: AgentEventPayload::TurnStarted,
            }],
            TurnCompleted(t) => {
                let env = codex_envelope(ctx.session_id.clone(), None, ctx.received_at);
                vec![
                    AgentEvent {
                        envelope: env.clone(),
                        payload: AgentEventPayload::Usage {
                            input_tokens: t.usage.input_tokens.max(0) as u64,
                            output_tokens: t.usage.output_tokens.max(0) as u64,
                            cache_read: t.usage.cached_input_tokens.max(0) as u64,
                            cache_create: 0,
                            cost_usd: None,
                        },
                    },
                    AgentEvent {
                        envelope: env,
                        payload: AgentEventPayload::TurnCompleted {
                            stop_reason: None,
                            is_error: false,
                        },
                    },
                ]
            }
            TurnFailed(t) => vec![AgentEvent {
                envelope: codex_envelope(ctx.session_id.clone(), None, ctx.received_at),
                payload: AgentEventPayload::TurnCompleted {
                    stop_reason: Some(t.error.message.clone()),
                    is_error: true,
                },
            }],
            ItemCompleted(e) => item_finalized(&e.item, ctx),
            ItemUpdated(e) => item_delta(&e.item, ctx),
            ItemStarted(_) => Vec::new(),
            Error(e) => vec![AgentEvent {
                envelope: codex_envelope(ctx.session_id.clone(), None, ctx.received_at),
                payload: AgentEventPayload::Error {
                    message: e.message.clone(),
                    fatal: true,
                },
            }],
        }
    }
}

fn item_delta(item: &stream::ThreadItem, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let env = codex_envelope(ctx.session_id.clone(), None, ctx.received_at);
    use stream::ThreadItemDetails::*;
    match &item.details {
        AgentMessage(m) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::AssistantTextDelta {
                content_block_index: 0,
                text_so_far: m.text.clone(),
            },
        }],
        Reasoning(r) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::ThinkingDelta {
                content_block_index: 0,
                text_so_far: r.text.clone(),
            },
        }],
        // CommandExecution / FileChange / McpToolCall / WebSearch / TodoList /
        // CollabToolCall / Error: in-progress tool states have no v1 *Delta
        // variant — would need a ToolResultDelta. Skip.
        _ => Vec::new(),
    }
}

fn item_finalized(item: &stream::ThreadItem, ctx: &ProjectionContext) -> Vec<AgentEvent> {
    let env = codex_envelope(ctx.session_id.clone(), None, ctx.received_at);
    use stream::ThreadItemDetails::*;
    match &item.details {
        AgentMessage(m) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::AssistantText { text: m.text.clone() },
        }],
        Reasoning(r) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::Thinking { text: r.text.clone() },
        }],
        CommandExecution(c) => {
            let success = matches!(c.status, stream::CommandExecutionStatus::Completed)
                && c.exit_code == Some(0);
            vec![
                AgentEvent {
                    envelope: env.clone(),
                    payload: AgentEventPayload::ToolUse {
                        id: item.id.clone(),
                        name: "command_execution".into(),
                        input: serde_json::json!({ "command": c.command }),
                    },
                },
                AgentEvent {
                    envelope: env,
                    payload: AgentEventPayload::ToolResult {
                        tool_use_id: item.id.clone(),
                        success,
                        text: Some(c.aggregated_output.clone()),
                        structured: Some(serde_json::json!({
                            "exit_code": c.exit_code,
                            "status": format!("{:?}", c.status).to_lowercase(),
                        })),
                    },
                },
            ]
        }
        FileChange(f) => vec![
            AgentEvent {
                envelope: env.clone(),
                payload: AgentEventPayload::ToolUse {
                    id: item.id.clone(),
                    name: "file_change".into(),
                    input: serde_json::to_value(&f.changes).unwrap_or(Value::Null),
                },
            },
            AgentEvent {
                envelope: env,
                payload: AgentEventPayload::ToolResult {
                    tool_use_id: item.id.clone(),
                    success: matches!(f.status, stream::PatchApplyStatus::Completed),
                    text: None,
                    structured: Some(serde_json::json!({
                        "status": format!("{:?}", f.status).to_lowercase()
                    })),
                },
            },
        ],
        McpToolCall(m) => {
            let success = matches!(m.status, stream::McpToolCallStatus::Completed);
            let text = m.error.as_ref().map(|e| e.message.clone());
            let structured = m.result.as_ref().and_then(|r| serde_json::to_value(r).ok());
            vec![
                AgentEvent {
                    envelope: env.clone(),
                    payload: AgentEventPayload::ToolUse {
                        id: item.id.clone(),
                        name: format!("mcp/{}/{}", m.server, m.tool),
                        input: m.arguments.clone(),
                    },
                },
                AgentEvent {
                    envelope: env,
                    payload: AgentEventPayload::ToolResult {
                        tool_use_id: item.id.clone(),
                        success,
                        text,
                        structured,
                    },
                },
            ]
        }
        WebSearch(w) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::ToolUse {
                id: item.id.clone(),
                name: "web_search".into(),
                input: serde_json::json!({ "query": w.query }),
            },
        }],
        TodoList(t) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::ToolUse {
                id: item.id.clone(),
                name: "todo_list".into(),
                input: serde_json::to_value(&t.items).unwrap_or(Value::Null),
            },
        }],
        CollabToolCall(c) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::ToolUse {
                id: item.id.clone(),
                name: format!("collab/{:?}", c.tool).to_lowercase(),
                input: serde_json::json!({
                    "sender_thread_id": c.sender_thread_id,
                    "receiver_thread_ids": c.receiver_thread_ids,
                    "prompt": c.prompt,
                }),
            },
        }],
        Error(e) => vec![AgentEvent {
            envelope: env,
            payload: AgentEventPayload::Error {
                message: e.message.clone(),
                fatal: false,
            },
        }],
    }
}

fn codex_envelope(
    session_id: String,
    raw_uuid: Option<String>,
    timestamp: DateTime<Utc>,
) -> AgentEventEnvelope {
    AgentEventEnvelope {
        session_id,
        timestamp,
        backend: Backend::Codex,
        raw_uuid,
        parent_tool_use_id: None,
    }
}

/// Project a [`RateLimitSnapshot`] into one [`RateLimit`] event per
/// populated window (primary, secondary). The envelope is cloned for
/// each event so callers pass the parent event's envelope as the
/// template. Same helper is used by the history projector (TokenCount
/// carries this) and the app-server projector
/// (`account/rateLimits/updated` carries this).
///
/// `status` is derived from the snapshot's `rate_limit_reached_type`:
/// `Some(_)` → `"rejected"` (binding limit hit); `None` → `"ok"`.
/// We can't synthesize `"warning"` for codex — the snapshot has no
/// such intermediate state, so callers wanting "near-limit" highlighting
/// should threshold on `used_percent` themselves.
pub(crate) fn rate_limit_events_from_snapshot(
    envelope: &AgentEventEnvelope,
    snap: &history::RateLimitSnapshot,
) -> Vec<AgentEvent> {
    let status = if snap.rate_limit_reached_type.is_some() {
        "rejected"
    } else {
        "ok"
    };
    let mut out = Vec::new();
    if let Some(w) = &snap.primary {
        out.push(rate_limit_event_from_window(envelope, "primary", w, status));
    }
    if let Some(w) = &snap.secondary {
        out.push(rate_limit_event_from_window(envelope, "secondary", w, status));
    }
    out
}

fn rate_limit_event_from_window(
    envelope: &AgentEventEnvelope,
    kind: &str,
    window: &history::RateLimitWindow,
    status: &str,
) -> AgentEvent {
    AgentEvent {
        envelope: envelope.clone(),
        payload: AgentEventPayload::RateLimit {
            limit_kind: kind.to_owned(),
            used_percent: Some(window.used_percent),
            status: Some(status.to_owned()),
            resets_at: window.resets_at,
            using_overage: None,
        },
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
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
    fn stream_full_run() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"th_1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}"#,
        ];
        let mut p = StreamProjector::new();
        let mut total = 0_usize;
        for l in lines {
            let ev: stream::CodexStreamEvent = serde_json::from_str(l).unwrap();
            total += p.project(&ev, &ctx()).len();
        }
        // SessionStarted + TurnStarted + AssistantText + Usage + TurnCompleted = 5
        assert_eq!(total, 5);
    }

    #[test]
    fn stream_command_execution_emits_tool_use_and_result() {
        let line = r#"{"type":"item.completed","item":{"id":"i","type":"command_execution","command":"ls","aggregated_output":"a\nb","exit_code":0,"status":"completed"}}"#;
        let ev: stream::CodexStreamEvent = serde_json::from_str(line).unwrap();
        let evs = StreamProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0].payload, AgentEventPayload::ToolUse { .. }));
        assert!(matches!(evs[1].payload, AgentEventPayload::ToolResult { .. }));
    }

    #[test]
    fn stream_item_updated_agent_message_emits_delta() {
        let line = r#"{"type":"item.updated","item":{"id":"i","type":"agent_message","text":"hello"}}"#;
        let ev: stream::CodexStreamEvent = serde_json::from_str(line).unwrap();
        let evs = StreamProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 1);
        match &evs[0].payload {
            AgentEventPayload::AssistantTextDelta { text_so_far, .. } => {
                assert_eq!(text_so_far, "hello");
            }
            _ => panic!("expected AssistantTextDelta"),
        }
    }

    #[test]
    fn history_token_count_emits_usage_then_rate_limits() {
        let line = r#"{"timestamp":"2026-05-10T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"input_tokens":100,"output_tokens":50,"cached_input_tokens":10},"rate_limits":{"primary":{"used_percent":75.5,"resets_at":1778414400},"secondary":{"used_percent":12.0},"rate_limit_reached_type":null}}}"#;
        let ev: history::CodexEvent = serde_json::from_str(line).unwrap();
        let evs = HistoryProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 3, "Usage + primary RateLimit + secondary RateLimit");
        assert!(matches!(evs[0].payload, AgentEventPayload::Usage { .. }));
        match &evs[1].payload {
            AgentEventPayload::RateLimit {
                limit_kind,
                used_percent,
                status,
                resets_at,
                ..
            } => {
                assert_eq!(limit_kind, "primary");
                assert!((used_percent.unwrap() - 75.5).abs() < 1e-9);
                assert_eq!(status.as_deref(), Some("ok"));
                assert_eq!(resets_at, &Some(1778414400));
            }
            _ => panic!("expected primary RateLimit"),
        }
        match &evs[2].payload {
            AgentEventPayload::RateLimit { limit_kind, used_percent, .. } => {
                assert_eq!(limit_kind, "secondary");
                assert!((used_percent.unwrap() - 12.0).abs() < 1e-9);
            }
            _ => panic!("expected secondary RateLimit"),
        }
    }

    #[test]
    fn history_token_count_rejected_status_when_limit_reached() {
        let line = r#"{"timestamp":"2026-05-10T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":{"primary":{"used_percent":100.0},"rate_limit_reached_type":"rate_limit_reached"}}}"#;
        let ev: history::CodexEvent = serde_json::from_str(line).unwrap();
        let evs = HistoryProjector::new().project(&ev, &ctx());
        // Usage + one RateLimit (primary only).
        assert_eq!(evs.len(), 2);
        match &evs[1].payload {
            AgentEventPayload::RateLimit { status, .. } => {
                assert_eq!(status.as_deref(), Some("rejected"));
            }
            _ => panic!("expected RateLimit"),
        }
    }

    #[test]
    fn history_function_call_parses_arguments_string() {
        let line = r#"{"timestamp":"2026-05-10T12:00:00Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#;
        let ev: history::CodexEvent = serde_json::from_str(line).unwrap();
        let evs = HistoryProjector::new().project(&ev, &ctx());
        assert_eq!(evs.len(), 1);
        match &evs[0].payload {
            AgentEventPayload::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(input.get("cmd").and_then(|v| v.as_str()), Some("ls"));
            }
            _ => panic!("expected ToolUse"),
        }
    }
}
