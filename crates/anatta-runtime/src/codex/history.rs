//! Faithful reproduction of codex CLI's rollout JSONL wire format.
//!
//! Empirically derived from real session files written by `codex-cli` 0.125.x
//! (May 2026). Each top-level event has shape `{type, timestamp, payload}`.
//!
//! Strict tagged-enum semantics: an unknown `type` or `payload.type` value
//! deliberately fails parsing so we notice schema drift.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ────────────────────────────────────────────────────────────────────────────
// Top level
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodexEvent {
    pub timestamp: String,
    #[serde(flatten)]
    pub kind: CodexEventKind,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum CodexEventKind {
    SessionMeta(SessionMeta),
    TurnContext(TurnContext),
    ResponseItem(ResponseItem),
    EventMsg(EventMsg),
    Compacted(Compacted),
}

// ────────────────────────────────────────────────────────────────────────────
// session_meta
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: String,
    pub originator: String,
    pub cli_version: String,
    pub timestamp: String,
    /// Absent on older sessions (pre-Sept 2025).
    #[serde(default)]
    pub model_provider: Option<String>,
    /// May be `null` in older sessions.
    #[serde(default)]
    pub source: Option<SessionSource>,
    /// Newer schema (post-rename); older sessions used [`Self::instructions`].
    #[serde(default)]
    pub base_instructions: Option<BaseInstructions>,
    /// Older schema field; superseded by `base_instructions`.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Free-form git context (branch, commit, dirty flag, etc.); shape varies.
    #[serde(default)]
    pub git: Option<Value>,
    #[serde(default)]
    pub agent_nickname: Option<String>,
    #[serde(default)]
    pub agent_role: Option<String>,
    #[serde(default)]
    pub forked_from_id: Option<String>,
    #[serde(default)]
    pub dynamic_tools: Option<Value>,
    #[serde(default)]
    pub memory_mode: Option<String>,
}

/// How a codex session was originated.
///
/// Older schema emits a flat origin string; newer schema emits a structured
/// `{subagent: ...}` for sessions spawned by another codex agent.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SessionSource {
    Origin(SessionOrigin),
    Subagent { subagent: SubagentInfo },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    Cli,
    Exec,
    Vscode,
    Unknown,
}

/// Subagent descriptor, observed in three shapes:
///   * a bare string label (e.g. `"review"`)
///   * an externally-tagged object `{thread_spawn: {...}}`
///   * an externally-tagged object `{other: "<name>"}`
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SubagentInfo {
    Plain(String),
    Tagged(SubagentDescriptor),
}

/// External-tag enum: JSON like `{thread_spawn: {...}}` or `{other: "..."}`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentDescriptor {
    ThreadSpawn(ThreadSpawn),
    Other(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThreadSpawn {
    pub parent_thread_id: String,
    pub depth: u32,
    pub agent_nickname: String,
    pub agent_role: String,
    /// Newer schema field; absent in older sessions.
    #[serde(default)]
    pub agent_path: Option<String>,
}

/// Base instructions can be a plain string or a wrapper `{text}` object,
/// depending on schema version.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum BaseInstructions {
    Plain(String),
    Wrapped { text: String },
}

// ────────────────────────────────────────────────────────────────────────────
// turn_context
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TurnContext {
    pub cwd: String,
    pub model: String,
    /// Approval policy: "untrusted", "on-failure", "on-request", "never"...
    pub approval_policy: String,
    /// Sandbox policy is a tagged enum on the wire; opaque here.
    pub sandbox_policy: Value,
    /// Added in newer schema versions.
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub current_date: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub personality: Option<String>,
    /// Older schemas: a bare string label. Newer: a structured spec.
    #[serde(default)]
    pub permission_profile: Option<PermissionProfile>,
    #[serde(default)]
    pub collaboration_mode: Option<Value>,
    #[serde(default)]
    pub realtime_active: Option<bool>,
    #[serde(default)]
    pub summary: Option<Value>,
    #[serde(default)]
    pub truncation_policy: Option<Value>,
    #[serde(default)]
    pub user_instructions: Option<String>,
    /// Newer schema fields.
    #[serde(default)]
    pub developer_instructions: Option<String>,
    #[serde(default)]
    pub final_output_json_schema: Option<Value>,
    #[serde(default)]
    pub file_system_sandbox_policy: Option<Value>,
}

// ────────────────────────────────────────────────────────────────────────────
// response_item — 8 variants
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    Message {
        role: String,
        content: Vec<MessageContent>,
        #[serde(default)]
        phase: Option<String>,
    },
    Reasoning {
        /// Array of reasoning blocks; shape varies (text, summary parts, ...).
        content: Value,
        summary: Value,
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        /// JSON-encoded string of arguments (OpenAI tool-call convention).
        arguments: String,
        /// Newer schema includes namespacing for tool resolution.
        #[serde(default)]
        namespace: Option<String>,
    },
    FunctionCallOutput {
        call_id: String,
        /// Tool result; tool-defined shape.
        output: Value,
    },
    CustomToolCall {
        call_id: String,
        name: String,
        input: Value,
        status: String,
    },
    CustomToolCallOutput {
        call_id: String,
        output: Value,
    },
    WebSearchCall {
        action: Value,
        status: String,
    },
    GhostSnapshot {
        ghost_commit: GhostCommitRef,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    InputText { text: String },
    OutputText { text: String },
    InputImage(InputImage),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InputImage {
    /// Image source: shape opaque (base64 / url / file ref).
    #[serde(flatten)]
    pub fields: Value,
}

// ────────────────────────────────────────────────────────────────────────────
// event_msg — 19 variants
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    TokenCount {
        info: Value,
        rate_limits: Value,
    },
    AgentReasoning {
        text: String,
    },
    AgentMessage {
        message: String,
        #[serde(default)]
        phase: Option<String>,
        #[serde(default)]
        memory_citation: Option<Value>,
    },
    UserMessage {
        message: String,
        #[serde(default)]
        images: Option<Value>,
        #[serde(default)]
        local_images: Option<Value>,
        #[serde(default)]
        text_elements: Option<Value>,
    },
    TaskStarted {
        collaboration_mode_kind: String,
        model_context_window: u64,
        /// Epoch seconds. Absent in older schemas.
        #[serde(default)]
        started_at: Option<i64>,
        #[serde(default)]
        turn_id: Option<String>,
    },
    TaskComplete {
        #[serde(default)]
        last_agent_message: Option<String>,
        #[serde(default)]
        turn_id: Option<String>,
        /// Epoch seconds. Absent in older schemas.
        #[serde(default)]
        completed_at: Option<i64>,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        time_to_first_token_ms: Option<u64>,
    },
    TurnAborted {
        reason: String,
        #[serde(default)]
        turn_id: Option<String>,
        #[serde(default)]
        completed_at: Option<i64>,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
    ExecCommandEnd {
        call_id: String,
        command: Vec<String>,
        cwd: String,
        duration: DurationSpec,
        exit_code: i32,
        process_id: ProcessId,
        status: String,
        source: String,
        stdout: String,
        stderr: String,
        aggregated_output: String,
        formatted_output: String,
        /// codex-internal parsed-command structure; not modeled here.
        parsed_cmd: Value,
        turn_id: String,
    },
    PatchApplyEnd {
        call_id: String,
        changes: Value,
        status: String,
        stdout: String,
        stderr: String,
        success: bool,
        turn_id: String,
    },
    GuardianAssessment {
        action: Value,
        id: String,
        status: String,
        target_item_id: String,
        turn_id: String,
        #[serde(default)]
        decision_source: Option<String>,
        #[serde(default)]
        rationale: Option<String>,
        #[serde(default)]
        risk_level: Option<String>,
        #[serde(default)]
        user_authorization: Option<Value>,
    },
    ThreadNameUpdated {
        thread_id: String,
        thread_name: String,
    },
    ContextCompacted {},
    WebSearchEnd {
        action: Value,
        call_id: String,
        query: String,
    },
    McpToolCallEnd {
        call_id: String,
        duration: Value,
        invocation: Value,
        result: Value,
    },
    ViewImageToolCall {
        call_id: String,
        path: String,
    },
    CollabAgentSpawnEnd {
        call_id: String,
        model: String,
        new_agent_nickname: String,
        new_agent_role: String,
        new_thread_id: String,
        prompt: String,
        reasoning_effort: String,
        sender_thread_id: String,
        status: String,
    },
    CollabAgentInteractionEnd {
        call_id: String,
        prompt: String,
        receiver_agent_nickname: String,
        receiver_agent_role: String,
        receiver_thread_id: String,
        sender_thread_id: String,
        status: String,
    },
    CollabCloseEnd {
        call_id: String,
        receiver_agent_nickname: String,
        receiver_agent_role: String,
        receiver_thread_id: String,
        sender_thread_id: String,
        status: CollabStatus,
    },
    CollabWaitingEnd {
        call_id: String,
        sender_thread_id: String,
        /// Map keyed by waited-on thread UUID. Inner status object is a
        /// codex-internal structure; left opaque here.
        statuses: HashMap<String, Value>,
        #[serde(default)]
        agent_statuses: Option<Value>,
    },
}

// ────────────────────────────────────────────────────────────────────────────
// compacted
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Compacted {
    pub message: String,
    /// Older sessions don't include this field.
    #[serde(default)]
    pub replacement_history: Option<Value>,
}

// ────────────────────────────────────────────────────────────────────────────
// Shared shapes used by multiple variants
// ────────────────────────────────────────────────────────────────────────────

/// Older schema: a bare commit hash string.
/// Newer schema: a structured object with parent + pre-existing untracked files.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum GhostCommitRef {
    Hash(String),
    Detail(GhostCommitDetail),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GhostCommitDetail {
    pub id: String,
    /// Null on the very first ghost snapshot (no parent yet).
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub preexisting_untracked_dirs: Vec<String>,
    #[serde(default)]
    pub preexisting_untracked_files: Vec<String>,
}

/// Older schema: a bare profile name string ("default", "managed", ...).
/// Newer schema: a fully-specified policy.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PermissionProfile {
    Named(String),
    Spec(PermissionProfileSpec),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionProfileSpec {
    Managed {
        /// Filesystem policy: codex-internal nested tree of
        /// `{type, file_system, network}` discriminators. Treated as opaque.
        file_system: Value,
        /// Network policy: "restricted", and possibly other future values.
        network: String,
    },
}

/// Status of a collaboration call.
///
/// Pending statuses are bare strings (e.g. `"running"`); completed ones
/// arrive as `{completed: <message>}`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CollabStatus {
    Pending(CollabPendingState),
    Completed { completed: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabPendingState {
    Running,
}

/// Process id is serialized as a string (current schema) but is permitted
/// to be a numeric value as well, defensively.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ProcessId {
    String(String),
    Number(u64),
}

/// `{secs, nanos}` — same shape as Rust's `std::time::Duration` serialization.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DurationSpec {
    pub secs: u64,
    pub nanos: u32,
}

// ────────────────────────────────────────────────────────────────────────────
// Unit tests with synthetic fixtures
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> CodexEvent {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("parse failed: {e}\nline: {s}"))
    }

    #[test]
    fn parse_session_meta_origin_string() {
        let line = r#"{"type":"session_meta","timestamp":"2026-05-05T20:17:08Z","payload":{"id":"abc","cwd":"/tmp","originator":"cli","cli_version":"0.125.0","timestamp":"2026-05-05T20:17:08Z","model_provider":"openai","source":"cli"}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::SessionMeta(SessionMeta {
                source: Some(SessionSource::Origin(SessionOrigin::Cli)), ..
            }) => {}
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn parse_session_meta_subagent_thread_spawn() {
        let line = r#"{"type":"session_meta","timestamp":"t","payload":{"id":"a","cwd":"/","originator":"o","cli_version":"v","timestamp":"t","model_provider":"openai","source":{"subagent":{"thread_spawn":{"parent_thread_id":"p","depth":1,"agent_nickname":"A","agent_role":"r"}}}}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::SessionMeta(SessionMeta {
                source: Some(SessionSource::Subagent {
                    subagent: SubagentInfo::Tagged(SubagentDescriptor::ThreadSpawn(t)),
                }),
                ..
            }) => assert_eq!(t.depth, 1),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn parse_session_meta_subagent_plain_string() {
        let line = r#"{"type":"session_meta","timestamp":"t","payload":{"id":"a","cwd":"/","originator":"o","cli_version":"v","timestamp":"t","model_provider":"openai","source":{"subagent":"review"}}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::SessionMeta(SessionMeta {
                source: Some(SessionSource::Subagent {
                    subagent: SubagentInfo::Plain(s),
                }),
                ..
            }) => assert_eq!(s, "review"),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn parse_session_meta_subagent_other() {
        let line = r#"{"type":"session_meta","timestamp":"t","payload":{"id":"a","cwd":"/","originator":"o","cli_version":"v","timestamp":"t","model_provider":"openai","source":{"subagent":{"other":"guardian"}}}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::SessionMeta(SessionMeta {
                source: Some(SessionSource::Subagent {
                    subagent: SubagentInfo::Tagged(SubagentDescriptor::Other(name)),
                }),
                ..
            }) => assert_eq!(name, "guardian"),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn parse_session_meta_source_null() {
        let line = r#"{"type":"session_meta","timestamp":"t","payload":{"id":"a","cwd":"/","originator":"o","cli_version":"v","timestamp":"t","model_provider":"openai","source":null}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::SessionMeta(SessionMeta { source: None, .. }) => {}
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn parse_ghost_snapshot_both_shapes() {
        let hash = r#"{"type":"response_item","timestamp":"t","payload":{"type":"ghost_snapshot","ghost_commit":"abc123"}}"#;
        let detail = r#"{"type":"response_item","timestamp":"t","payload":{"type":"ghost_snapshot","ghost_commit":{"id":"abc","parent":"def","preexisting_untracked_dirs":[],"preexisting_untracked_files":[]}}}"#;
        let h = parse(hash);
        let d = parse(detail);
        match h.kind {
            CodexEventKind::ResponseItem(ResponseItem::GhostSnapshot {
                ghost_commit: GhostCommitRef::Hash(s),
            }) => assert_eq!(s, "abc123"),
            other => panic!("wrong shape: {other:?}"),
        }
        match d.kind {
            CodexEventKind::ResponseItem(ResponseItem::GhostSnapshot {
                ghost_commit: GhostCommitRef::Detail(c),
            }) => assert_eq!(c.id, "abc"),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn parse_collab_close_status_both_shapes() {
        let running = r#"{"type":"event_msg","timestamp":"t","payload":{"type":"collab_close_end","call_id":"c","receiver_agent_nickname":"a","receiver_agent_role":"r","receiver_thread_id":"t","sender_thread_id":"s","status":"running"}}"#;
        let done = r#"{"type":"event_msg","timestamp":"t","payload":{"type":"collab_close_end","call_id":"c","receiver_agent_nickname":"a","receiver_agent_role":"r","receiver_thread_id":"t","sender_thread_id":"s","status":{"completed":"hi"}}}"#;
        let r = parse(running);
        let d = parse(done);
        match r.kind {
            CodexEventKind::EventMsg(EventMsg::CollabCloseEnd { status: CollabStatus::Pending(CollabPendingState::Running), .. }) => {}
            other => panic!("wrong shape: {other:?}"),
        }
        match d.kind {
            CodexEventKind::EventMsg(EventMsg::CollabCloseEnd { status: CollabStatus::Completed { completed }, .. }) => {
                assert_eq!(completed, "hi");
            }
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn parse_event_msg_token_count() {
        let line = r#"{"type":"event_msg","timestamp":"t","payload":{"type":"token_count","info":{},"rate_limits":{}}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::EventMsg(EventMsg::TokenCount { .. }) => {}
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_response_item_function_call() {
        let line = r#"{"type":"response_item","timestamp":"t","payload":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::ResponseItem(ResponseItem::FunctionCall {
                call_id, name, arguments, ..
            }) => {
                assert_eq!(call_id, "c1");
                assert_eq!(name, "shell");
                assert!(arguments.contains("ls"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_response_item_message() {
        let line = r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#;
        let ev = parse(line);
        match ev.kind {
            CodexEventKind::ResponseItem(ResponseItem::Message { role, content, .. }) => {
                assert_eq!(role, "user");
                assert_eq!(content.len(), 1);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unknown_top_type_fails() {
        let line = r#"{"type":"never_seen","timestamp":"t","payload":{}}"#;
        assert!(serde_json::from_str::<CodexEvent>(line).is_err());
    }

    #[test]
    fn unknown_event_msg_subtype_fails() {
        let line = r#"{"type":"event_msg","timestamp":"t","payload":{"type":"never_seen"}}"#;
        assert!(serde_json::from_str::<CodexEvent>(line).is_err());
    }
}
