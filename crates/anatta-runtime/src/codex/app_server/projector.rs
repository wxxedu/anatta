//! Project codex app-server `ServerNotification`s into `AgentEvent`s.
//!
//! The projector is stateful: codex emits `item/agentMessage/delta`
//! with **incremental** chunks (just the new text piece), while our
//! `AgentEventPayload::AssistantTextDelta` carries **snapshot**
//! (`text_so_far`) semantics. So we accumulate text per `item_id` and
//! emit the running total each time a chunk arrives.
//!
//! Method name → AgentEvent mapping (only the subset we render):
//!
//! | server notification              | AgentEvent                         |
//! |---|---|
//! | `thread/started`                 | `SessionStarted` (deduped — already emitted at spawn) |
//! | `turn/started`                   | `TurnStarted`                      |
//! | `item/agentMessage/delta`        | `AssistantTextDelta`               |
//! | `item/completed` (agentMessage)  | `AssistantText`                    |
//! | `item/reasoning/textDelta`       | `ThinkingDelta`                    |
//! | `item/completed` (reasoning)     | `Thinking`                         |
//! | `item/started` (commandExecution)| `ToolUse`                          |
//! | `item/completed` (commandExecution)| `ToolResult`                    |
//! | `item/started` (fileChange)      | `ToolUse`                          |
//! | `item/completed` (fileChange)    | `ToolResult`                       |
//! | `item/started` (mcpToolCall)     | `ToolUse`                          |
//! | `item/completed` (mcpToolCall)   | `ToolResult`                       |
//! | `item/started` (webSearch)       | `ToolUse` (no ToolResult)          |
//! | `thread/tokenUsage/updated`      | `Usage`                            |
//! | `turn/completed`                 | `TurnCompleted`                    |
//! | `error`                          | `Error { fatal: !willRetry }`      |
//! | `warning`                        | `Error { fatal: false }`           |
//!
//! Unrecognized methods are silently dropped — codex emits many
//! lifecycle notifications (`mcpServer/...`, `thread/status/changed`,
//! `account/...`) that are not user-facing.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use anatta_core::{AgentEvent, AgentEventEnvelope, AgentEventPayload, Backend};

/// Stateful per-turn projector.
pub(crate) struct AppServerProjector {
    thread_id: String,
    /// Accumulated agent message text per item_id, so we can emit the
    /// snapshot (`text_so_far`) required by `AssistantTextDelta`.
    agent_message_text: HashMap<String, String>,
    /// Same for reasoning text per (item_id, content_index).
    reasoning_text: HashMap<(String, u32), String>,
}

impl AppServerProjector {
    pub(crate) fn new(thread_id: String) -> Self {
        Self {
            thread_id,
            agent_message_text: HashMap::new(),
            reasoning_text: HashMap::new(),
        }
    }

    /// Dispatch one notification. Returns the AgentEvents to forward.
    pub(crate) fn project(&mut self, method: &str, params: &Value) -> Vec<AgentEvent> {
        let now = Utc::now();
        match method {
            "turn/started" => vec![self.event(now, AgentEventPayload::TurnStarted)],

            "item/agentMessage/delta" => self.project_agent_message_delta(params, now),

            "item/reasoning/textDelta" => self.project_reasoning_delta(params, now),

            "item/started" => self.project_item_started(params, now),

            "item/completed" => self.project_item_completed(params, now),

            "thread/tokenUsage/updated" => self.project_token_usage(params, now),

            "turn/completed" => self.project_turn_completed(params, now),

            "error" => self.project_error(params, now, /* fatal default */ true),

            "warning" => self.project_warning(params, now),

            // thread/started is informational; the spawn driver emits
            // SessionStarted itself from the response data.
            "thread/started"
            | "thread/status/changed"
            | "thread/closed"
            | "thread/archived"
            | "thread/unarchived"
            | "thread/name/updated"
            | "thread/compacted"
            | "turn/plan/updated"
            | "turn/diff/updated"
            | "mcpServer/startupStatus/updated"
            | "mcpServer/oauthLogin/completed"
            | "account/updated"
            | "account/rateLimits/updated"
            | "account/login/completed"
            | "model/rerouted"
            | "model/verification"
            | "serverRequest/resolved"
            | "configWarning"
            | "deprecationNotice"
            | "guardianWarning"
            | "fs/changed"
            | "skills/changed"
            | "app/list/updated"
            | "externalAgentConfig/import/completed"
            | "hook/started"
            | "hook/completed"
            // Reasoning summary is supplementary to text; skip in v1.
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded"
            // commandExecution stdout/stderr deltas — we could surface
            // them under the tool anchor but the renderer is keyed
            // off ToolResult, which arrives with aggregatedOutput.
            // Skip in v1 to avoid double-display.
            | "command/exec/outputDelta"
            | "item/commandExecution/outputDelta"
            | "item/commandExecution/terminalInteraction"
            | "item/fileChange/outputDelta"
            | "item/fileChange/patchUpdated"
            | "item/plan/delta"
            | "item/mcpToolCall/progress"
            | "item/autoApprovalReview/started"
            | "item/autoApprovalReview/completed"
            // Realtime / windows / fuzzy / etc — never seen in
            // standard exec flow.
            | _ => Vec::new(),
        }
    }

    fn event(&self, ts: DateTime<Utc>, payload: AgentEventPayload) -> AgentEvent {
        AgentEvent {
            envelope: AgentEventEnvelope {
                session_id: self.thread_id.clone(),
                timestamp: ts,
                backend: Backend::Codex,
                raw_uuid: None,
                parent_tool_use_id: None,
            },
            payload,
        }
    }

    fn project_agent_message_delta(
        &mut self,
        params: &Value,
        now: DateTime<Utc>,
    ) -> Vec<AgentEvent> {
        let Some(p) = parse::<AgentMessageDelta>(params) else {
            return Vec::new();
        };
        let acc = self
            .agent_message_text
            .entry(p.item_id.clone())
            .or_default();
        acc.push_str(&p.delta);
        let text_so_far = acc.clone();
        vec![self.event(
            now,
            AgentEventPayload::AssistantTextDelta {
                content_block_index: 0,
                text_so_far,
            },
        )]
    }

    fn project_reasoning_delta(
        &mut self,
        params: &Value,
        now: DateTime<Utc>,
    ) -> Vec<AgentEvent> {
        let Some(p) = parse::<ReasoningTextDelta>(params) else {
            return Vec::new();
        };
        let acc = self
            .reasoning_text
            .entry((p.item_id.clone(), p.content_index))
            .or_default();
        acc.push_str(&p.delta);
        let text_so_far = acc.clone();
        vec![self.event(
            now,
            AgentEventPayload::ThinkingDelta {
                content_block_index: p.content_index,
                text_so_far,
            },
        )]
    }

    fn project_item_started(&mut self, params: &Value, now: DateTime<Utc>) -> Vec<AgentEvent> {
        let item = match params.get("item") {
            Some(i) => i,
            None => return Vec::new(),
        };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match item_type {
            // commandExecution: emit ToolUse so the renderer can show
            // `⚙ command_execution(command=...)` immediately. The matching
            // ToolResult fires on item/completed.
            "commandExecution" => {
                let command = item
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                vec![self.event(
                    now,
                    AgentEventPayload::ToolUse {
                        id,
                        name: "command_execution".into(),
                        input: serde_json::json!({ "command": command }),
                    },
                )]
            }
            "fileChange" => {
                let changes = item
                    .get("changes")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                vec![self.event(
                    now,
                    AgentEventPayload::ToolUse {
                        id,
                        name: "file_change".into(),
                        input: changes,
                    },
                )]
            }
            "mcpToolCall" => {
                let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("");
                let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = item
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                vec![self.event(
                    now,
                    AgentEventPayload::ToolUse {
                        id,
                        name: format!("mcp/{server}/{tool}"),
                        input: arguments,
                    },
                )]
            }
            "webSearch" => {
                let query = item
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                vec![self.event(
                    now,
                    AgentEventPayload::ToolUse {
                        id,
                        name: "web_search".into(),
                        input: serde_json::json!({ "query": query }),
                    },
                )]
            }
            // agentMessage, reasoning, plan, userMessage, etc all emit
            // their content via deltas + item/completed. No start event.
            _ => Vec::new(),
        }
    }

    fn project_item_completed(
        &mut self,
        params: &Value,
        now: DateTime<Utc>,
    ) -> Vec<AgentEvent> {
        let item = match params.get("item") {
            Some(i) => i,
            None => return Vec::new(),
        };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");

        match item_type {
            "agentMessage" => {
                let text = item
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.agent_message_text.remove(id);
                vec![self.event(now, AgentEventPayload::AssistantText { text })]
            }
            "reasoning" => {
                // Clean up any accumulated reasoning state for this id
                self.reasoning_text.retain(|(k, _), _| k != id);
                let text = item
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if text.is_empty() {
                    return Vec::new();
                }
                vec![self.event(now, AgentEventPayload::Thinking { text })]
            }
            "commandExecution" => {
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("failed");
                let exit_code = item.get("exitCode").and_then(|v| v.as_i64());
                let success = status == "completed" && exit_code == Some(0);
                let text = item
                    .get("aggregatedOutput")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let structured = Some(serde_json::json!({
                    "exit_code": exit_code,
                    "status": status,
                }));
                vec![self.event(
                    now,
                    AgentEventPayload::ToolResult {
                        tool_use_id: id.to_string(),
                        success,
                        text,
                        structured,
                    },
                )]
            }
            "fileChange" => {
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("failed");
                let success = status == "completed";
                let structured = Some(serde_json::json!({ "status": status }));
                vec![self.event(
                    now,
                    AgentEventPayload::ToolResult {
                        tool_use_id: id.to_string(),
                        success,
                        text: None,
                        structured,
                    },
                )]
            }
            "mcpToolCall" => {
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("failed");
                let success = status == "completed";
                let text = item
                    .get("error")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let structured = item.get("result").cloned();
                vec![self.event(
                    now,
                    AgentEventPayload::ToolResult {
                        tool_use_id: id.to_string(),
                        success,
                        text,
                        structured,
                    },
                )]
            }
            _ => Vec::new(),
        }
    }

    fn project_token_usage(&mut self, params: &Value, now: DateTime<Utc>) -> Vec<AgentEvent> {
        let last = params.get("tokenUsage").and_then(|u| u.get("last"));
        let Some(last) = last else { return Vec::new() };
        let input = last
            .get("inputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = last
            .get("outputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = last
            .get("cachedInputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        vec![self.event(
            now,
            AgentEventPayload::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read,
                cache_create: 0,
                cost_usd: None,
            },
        )]
    }

    fn project_turn_completed(
        &mut self,
        params: &Value,
        now: DateTime<Utc>,
    ) -> Vec<AgentEvent> {
        let status = params
            .get("turn")
            .and_then(|t| t.get("status"))
            .and_then(|v| v.as_str())
            .unwrap_or("completed");
        let is_error = status == "failed" || status == "interrupted";
        vec![self.event(
            now,
            AgentEventPayload::TurnCompleted {
                stop_reason: Some(status.to_string()),
                is_error,
            },
        )]
    }

    fn project_error(
        &mut self,
        params: &Value,
        now: DateTime<Utc>,
        fatal_default: bool,
    ) -> Vec<AgentEvent> {
        let message = params
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown error)")
            .to_string();
        let will_retry = params
            .get("willRetry")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        vec![self.event(
            now,
            AgentEventPayload::Error {
                message,
                fatal: fatal_default && !will_retry,
            },
        )]
    }

    fn project_warning(&mut self, params: &Value, now: DateTime<Utc>) -> Vec<AgentEvent> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(warning)")
            .to_string();
        vec![self.event(
            now,
            AgentEventPayload::Error {
                message,
                fatal: false,
            },
        )]
    }
}

fn parse<'de, T: Deserialize<'de>>(v: &'de Value) -> Option<T> {
    T::deserialize(v).ok()
}

// ──────────────────────────────────────────────────────────────────────
// Notification param shapes (only the ones we read fields off of)
// ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AgentMessageDelta {
    #[serde(rename = "itemId")]
    item_id: String,
    delta: String,
}

#[derive(Deserialize)]
struct ReasoningTextDelta {
    #[serde(rename = "itemId")]
    item_id: String,
    #[serde(rename = "contentIndex")]
    content_index: u32,
    delta: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(p: &mut AppServerProjector, method: &str, params: Value) -> Vec<AgentEventPayload> {
        p.project(method, &params)
            .into_iter()
            .map(|e| e.payload)
            .collect()
    }

    #[test]
    fn agent_message_delta_accumulates() {
        let mut p = AppServerProjector::new("t1".into());
        let a = run(
            &mut p,
            "item/agentMessage/delta",
            json!({"itemId": "i1", "threadId": "t1", "turnId": "u1", "delta": "Hel"}),
        );
        let b = run(
            &mut p,
            "item/agentMessage/delta",
            json!({"itemId": "i1", "threadId": "t1", "turnId": "u1", "delta": "lo"}),
        );
        match (&a[0], &b[0]) {
            (
                AgentEventPayload::AssistantTextDelta {
                    text_so_far: first,
                    ..
                },
                AgentEventPayload::AssistantTextDelta {
                    text_so_far: second,
                    ..
                },
            ) => {
                assert_eq!(first, "Hel");
                assert_eq!(second, "Hello");
            }
            _ => panic!("expected two AssistantTextDelta events"),
        }
    }

    #[test]
    fn agent_message_completed_emits_final() {
        let mut p = AppServerProjector::new("t1".into());
        let evts = run(
            &mut p,
            "item/completed",
            json!({
                "item": {"type": "agentMessage", "id": "i1", "text": "Hello world"},
                "threadId": "t1",
                "turnId": "u1",
            }),
        );
        assert_eq!(evts.len(), 1);
        match &evts[0] {
            AgentEventPayload::AssistantText { text } => assert_eq!(text, "Hello world"),
            _ => panic!("expected AssistantText"),
        }
    }

    #[test]
    fn command_execution_emits_tool_use_then_result() {
        let mut p = AppServerProjector::new("t1".into());
        let starts = run(
            &mut p,
            "item/started",
            json!({
                "item": {
                    "type": "commandExecution",
                    "id": "i7",
                    "command": "ls -la",
                    "cwd": "/tmp",
                    "status": "inProgress",
                    "commandActions": []
                },
                "threadId": "t1", "turnId": "u1",
            }),
        );
        assert!(matches!(starts[0], AgentEventPayload::ToolUse { .. }));

        let completes = run(
            &mut p,
            "item/completed",
            json!({
                "item": {
                    "type": "commandExecution",
                    "id": "i7",
                    "command": "ls -la",
                    "cwd": "/tmp",
                    "status": "completed",
                    "exitCode": 0,
                    "aggregatedOutput": "file1\nfile2",
                    "commandActions": []
                },
                "threadId": "t1", "turnId": "u1",
            }),
        );
        match &completes[0] {
            AgentEventPayload::ToolResult { success, text, .. } => {
                assert!(*success);
                assert_eq!(text.as_deref(), Some("file1\nfile2"));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn turn_completed_emits_turn_completed() {
        let mut p = AppServerProjector::new("t1".into());
        let evts = run(
            &mut p,
            "turn/completed",
            json!({"threadId":"t1","turn":{"id":"u1","status":"completed","items":[]}}),
        );
        match &evts[0] {
            AgentEventPayload::TurnCompleted {
                stop_reason,
                is_error,
            } => {
                assert_eq!(stop_reason.as_deref(), Some("completed"));
                assert!(!*is_error);
            }
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn turn_failed_emits_error_turn_completed() {
        let mut p = AppServerProjector::new("t1".into());
        let evts = run(
            &mut p,
            "turn/completed",
            json!({"threadId":"t1","turn":{"id":"u1","status":"failed","items":[],"error":{"message":"x"}}}),
        );
        match &evts[0] {
            AgentEventPayload::TurnCompleted { is_error, .. } => assert!(*is_error),
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn token_usage_emits_usage_with_last_breakdown() {
        let mut p = AppServerProjector::new("t1".into());
        let evts = run(
            &mut p,
            "thread/tokenUsage/updated",
            json!({
                "threadId": "t1",
                "turnId": "u1",
                "tokenUsage": {
                    "last": {
                        "inputTokens": 100,
                        "outputTokens": 25,
                        "cachedInputTokens": 50,
                        "reasoningOutputTokens": 5,
                        "totalTokens": 125
                    },
                    "total": {
                        "inputTokens": 100,
                        "outputTokens": 25,
                        "cachedInputTokens": 50,
                        "reasoningOutputTokens": 5,
                        "totalTokens": 125
                    }
                }
            }),
        );
        match &evts[0] {
            AgentEventPayload::Usage {
                input_tokens,
                output_tokens,
                cache_read,
                ..
            } => {
                assert_eq!(*input_tokens, 100);
                assert_eq!(*output_tokens, 25);
                assert_eq!(*cache_read, 50);
            }
            _ => panic!("expected Usage"),
        }
    }

    #[test]
    fn error_notification_maps_to_fatal_error_event() {
        let mut p = AppServerProjector::new("t1".into());
        let evts = run(
            &mut p,
            "error",
            json!({"threadId":"t1","turnId":"u1","willRetry":false,"error":{"message":"boom"}}),
        );
        match &evts[0] {
            AgentEventPayload::Error { message, fatal } => {
                assert_eq!(message, "boom");
                assert!(*fatal);
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn warning_notification_maps_to_nonfatal_error_event() {
        let mut p = AppServerProjector::new("t1".into());
        let evts = run(
            &mut p,
            "warning",
            json!({"message":"slow network"}),
        );
        match &evts[0] {
            AgentEventPayload::Error { message, fatal } => {
                assert_eq!(message, "slow network");
                assert!(!*fatal);
            }
            _ => panic!("expected non-fatal Error"),
        }
    }

    #[test]
    fn unknown_notification_is_silent() {
        let mut p = AppServerProjector::new("t1".into());
        let evts = run(&mut p, "mcpServer/startupStatus/updated", json!({}));
        assert!(evts.is_empty());
    }
}
