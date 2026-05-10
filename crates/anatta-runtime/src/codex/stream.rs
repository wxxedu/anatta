//! Faithful reproduction of Codex CLI's stdout streaming protocol.
//!
//! Emitted by `codex exec --json`. Higher-level thread/turn/item model.
//! Distinct from the disk rollout schema parsed in [`super::history`].
//!
//! Schema mirrored verbatim from upstream `openai/codex` at git tag
//! `rust-v0.125.0`, source path `codex-rs/exec/src/exec_events.rs`.
//! Three fields are typed as `serde_json::Value` because upstream
//! itself does so — they are intentionally open-shape (MCP arguments,
//! MCP content blocks).
//!
//! Strict tagged-enum semantics: an unknown `type` deliberately fails
//! parsing so we notice schema drift.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ────────────────────────────────────────────────────────────────────────────
// Top level
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum CodexStreamEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted(ThreadStartedEvent),

    #[serde(rename = "turn.started")]
    TurnStarted(TurnStartedEvent),

    #[serde(rename = "turn.completed")]
    TurnCompleted(TurnCompletedEvent),

    #[serde(rename = "turn.failed")]
    TurnFailed(TurnFailedEvent),

    #[serde(rename = "item.started")]
    ItemStarted(ItemEvent),

    #[serde(rename = "item.updated")]
    ItemUpdated(ItemEvent),

    #[serde(rename = "item.completed")]
    ItemCompleted(ItemEvent),

    /// Unrecoverable stream-level error (flat: `{type: "error", message: "..."}`).
    #[serde(rename = "error")]
    Error(ThreadErrorEvent),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThreadStartedEvent {
    pub thread_id: String,
}

/// `turn.started` carries no fields (`TurnStartedEvent` is `{}` upstream).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TurnStartedEvent {}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TurnCompletedEvent {
    pub usage: Usage,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TurnFailedEvent {
    pub error: ThreadErrorEvent,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThreadErrorEvent {
    pub message: String,
}

/// `item.started` / `item.updated` / `item.completed` all carry `{ item: ThreadItem }`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ItemEvent {
    pub item: ThreadItem,
}

// ────────────────────────────────────────────────────────────────────────────
// Usage (matches upstream `Usage` exactly: 4 required i64 fields)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    /// Added in v0.125.0 (PR #19308). Required from that version onward.
    pub reasoning_output_tokens: i64,
}

// ────────────────────────────────────────────────────────────────────────────
// ThreadItem + the 9 ThreadItemDetails variants
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThreadItem {
    pub id: String,
    #[serde(flatten)]
    pub details: ThreadItemDetails,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadItemDetails {
    AgentMessage(AgentMessageItem),
    Reasoning(ReasoningItem),
    CommandExecution(CommandExecutionItem),
    FileChange(FileChangeItem),
    McpToolCall(McpToolCallItem),
    CollabToolCall(CollabToolCallItem),
    WebSearch(WebSearchItem),
    TodoList(TodoListItem),
    /// Non-fatal error surfaced as an item (vs the top-level `error` ThreadEvent).
    Error(ErrorItem),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentMessageItem {
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReasoningItem {
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommandExecutionItem {
    pub command: String,
    pub aggregated_output: String,
    /// Null while the command is still running.
    pub exit_code: Option<i32>,
    pub status: CommandExecutionStatus,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
    /// Sandbox / approval policy rejected the command.
    Declined,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileChangeItem {
    pub changes: Vec<FileUpdateChange>,
    pub status: PatchApplyStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchChangeKind {
    Add,
    Delete,
    Update,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchApplyStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolCallItem {
    pub server: String,
    pub tool: String,
    /// MCP arguments are intentionally open-shape upstream.
    #[serde(default)]
    pub arguments: Value,
    pub result: Option<McpToolCallItemResult>,
    pub error: Option<McpToolCallItemError>,
    pub status: McpToolCallStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolCallItemResult {
    /// Raw MCP content blocks; intentionally open-shape upstream.
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpToolCallItemError {
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolCallStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CollabToolCallItem {
    pub tool: CollabTool,
    pub sender_thread_id: String,
    pub receiver_thread_ids: Vec<String>,
    pub prompt: Option<String>,
    pub agents_states: HashMap<String, CollabAgentState>,
    pub status: CollabToolCallStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabTool {
    SpawnAgent,
    SendInput,
    Wait,
    CloseAgent,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabToolCallStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CollabAgentState {
    pub status: CollabAgentStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabAgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebSearchItem {
    /// Upstream defines an inner `id` field, but `ThreadItem`'s
    /// `#[serde(flatten)]` consumes the JSON `id` first, so the inner
    /// is unreachable in practice. Marked optional for resilience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub query: String,
    pub action: WebSearchAction,
}

/// Mirrors `codex_protocol::models::WebSearchAction`. All inner fields are
/// optional; the model may omit `query` etc. The `Other` arm is for forward
/// compatibility with future actions added by the Responses API.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queries: Option<Vec<String>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// Catch-all for forward compatibility with future Responses-API actions.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TodoListItem {
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TodoItem {
    pub text: String,
    pub completed: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorItem {
    pub message: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> CodexStreamEvent {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("parse failed: {e}\nline: {s}"))
    }

    #[test]
    fn parses_thread_started() {
        let line = r#"{"type":"thread.started","thread_id":"019e11a4-ba62-7681-8756-ab48a7cf200d"}"#;
        match parse(line) {
            CodexStreamEvent::ThreadStarted(e) => {
                assert_eq!(e.thread_id, "019e11a4-ba62-7681-8756-ab48a7cf200d");
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_turn_started_empty() {
        let line = r#"{"type":"turn.started"}"#;
        match parse(line) {
            CodexStreamEvent::TurnStarted(_) => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_turn_completed_with_usage() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":33561,"cached_input_tokens":3456,"output_tokens":29,"reasoning_output_tokens":22}}"#;
        match parse(line) {
            CodexStreamEvent::TurnCompleted(e) => {
                assert_eq!(e.usage.input_tokens, 33561);
                assert_eq!(e.usage.cached_input_tokens, 3456);
                assert_eq!(e.usage.output_tokens, 29);
                assert_eq!(e.usage.reasoning_output_tokens, 22);
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_turn_failed() {
        let line = r#"{"type":"turn.failed","error":{"message":"boom"}}"#;
        match parse(line) {
            CodexStreamEvent::TurnFailed(e) => assert_eq!(e.error.message, "boom"),
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_top_level_error() {
        let line = r#"{"type":"error","message":"unrecoverable"}"#;
        match parse(line) {
            CodexStreamEvent::Error(e) => assert_eq!(e.message, "unrecoverable"),
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_item_completed_agent_message() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#;
        match parse(line) {
            CodexStreamEvent::ItemCompleted(e) => {
                assert_eq!(e.item.id, "item_0");
                match e.item.details {
                    ThreadItemDetails::AgentMessage(m) => assert_eq!(m.text, "OK"),
                    other => panic!("wrong details: {other:?}"),
                }
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_item_completed_reasoning() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"reasoning","text":"think"}}"#;
        match parse(line) {
            CodexStreamEvent::ItemCompleted(e) => match e.item.details {
                ThreadItemDetails::Reasoning(r) => assert_eq!(r.text, "think"),
                other => panic!("wrong details: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_command_execution_item() {
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"ls","aggregated_output":"file\n","exit_code":0,"status":"completed"}}"#;
        match parse(line) {
            CodexStreamEvent::ItemCompleted(e) => match e.item.details {
                ThreadItemDetails::CommandExecution(c) => {
                    assert_eq!(c.command, "ls");
                    assert_eq!(c.exit_code, Some(0));
                    matches!(c.status, CommandExecutionStatus::Completed);
                }
                other => panic!("wrong details: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_command_execution_in_progress_no_exit_code() {
        let line = r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"sleep 1","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
        let ev = parse(line);
        assert!(matches!(ev, CodexStreamEvent::ItemStarted(_)));
    }

    #[test]
    fn parses_file_change_item() {
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/x","kind":"add"},{"path":"/y","kind":"update"}],"status":"completed"}}"#;
        match parse(line) {
            CodexStreamEvent::ItemCompleted(e) => match e.item.details {
                ThreadItemDetails::FileChange(f) => {
                    assert_eq!(f.changes.len(), 2);
                    matches!(f.changes[0].kind, PatchChangeKind::Add);
                    matches!(f.changes[1].kind, PatchChangeKind::Update);
                }
                other => panic!("wrong details: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_mcp_tool_call_with_open_shape_arguments() {
        let line = r#"{"type":"item.completed","item":{"id":"item_4","type":"mcp_tool_call","server":"git","tool":"status","arguments":{"any":["thing",1,true]},"result":null,"error":null,"status":"completed"}}"#;
        match parse(line) {
            CodexStreamEvent::ItemCompleted(e) => match e.item.details {
                ThreadItemDetails::McpToolCall(m) => {
                    assert_eq!(m.server, "git");
                    assert_eq!(m.tool, "status");
                    assert!(m.arguments.is_object());
                }
                other => panic!("wrong details: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_web_search_item_with_action_search() {
        // Upstream `WebSearchItem` declares its own `id` field but `ThreadItem`
        // is `#[serde(flatten)]`-ed over it, so both Rust fields read the
        // single JSON `id`.
        let line = r#"{"type":"item.completed","item":{"id":"item_5","type":"web_search","query":"hello","action":{"type":"search","query":"hello"}}}"#;
        match parse(line) {
            CodexStreamEvent::ItemCompleted(e) => match e.item.details {
                ThreadItemDetails::WebSearch(w) => {
                    assert_eq!(w.query, "hello");
                    matches!(w.action, WebSearchAction::Search { .. });
                }
                other => panic!("wrong details: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_web_search_action_other_via_unknown_variant() {
        let line = r#"{"type":"item.completed","item":{"id":"item_5","type":"web_search","query":"q","action":{"type":"future_thing"}}}"#;
        // Both outer ThreadItem.id and inner WebSearchItem.id read from the same JSON "id".
        match parse(line) {
            CodexStreamEvent::ItemCompleted(e) => match e.item.details {
                ThreadItemDetails::WebSearch(w) => {
                    matches!(w.action, WebSearchAction::Other);
                }
                other => panic!("wrong details: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_todo_list_item() {
        let line = r#"{"type":"item.updated","item":{"id":"item_6","type":"todo_list","items":[{"text":"a","completed":false},{"text":"b","completed":true}]}}"#;
        match parse(line) {
            CodexStreamEvent::ItemUpdated(e) => match e.item.details {
                ThreadItemDetails::TodoList(t) => {
                    assert_eq!(t.items.len(), 2);
                    assert!(t.items[1].completed);
                }
                other => panic!("wrong details: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn unknown_top_level_type_fails() {
        let line = r#"{"type":"never.seen","x":1}"#;
        assert!(serde_json::from_str::<CodexStreamEvent>(line).is_err());
    }

    #[test]
    fn unknown_item_type_fails() {
        let line = r#"{"type":"item.completed","item":{"id":"x","type":"never_seen"}}"#;
        assert!(serde_json::from_str::<CodexStreamEvent>(line).is_err());
    }

    #[test]
    fn parses_full_recorded_run() {
        // Captured from `codex exec --json "Say only OK"`
        let lines = [
            r#"{"type":"thread.started","thread_id":"019e11a4-ba62-7681-8756-ab48a7cf200d"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":33561,"cached_input_tokens":3456,"output_tokens":29,"reasoning_output_tokens":22}}"#,
        ];
        for l in lines {
            let _ = parse(l);
        }
    }
}
