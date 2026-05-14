//! Faithful reproduction of Claude Code CLI's session JSONL wire format.
//!
//! Empirically derived from real session files written by `claude` 2.1.x
//! (May 2026) across the developer's project history. Each line in a
//! Claude Code session JSONL maps to exactly one [`ClaudeEvent`].
//!
//! Strict tagged-enum semantics throughout: an unknown `type` /
//! `subtype` / `attachment.type` / content-block `type` deliberately
//! fails parsing so we notice schema drift.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ────────────────────────────────────────────────────────────────────────────
// Top level
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)]
pub enum ClaudeEvent {
    Assistant(AssistantEvent),
    User(UserEvent),
    System(SystemEvent),
    Attachment(AttachmentEvent),
    Progress(ProgressEvent),
    QueueOperation(QueueOperationEvent),
    LastPrompt(LastPromptEvent),
    PermissionMode(PermissionModeEvent),
    AiTitle(AiTitleEvent),
    CustomTitle(CustomTitleEvent),
    FileHistorySnapshot(FileHistorySnapshotEvent),
    PrLink(PrLinkEvent),
    WorktreeState(WorktreeStateEvent),
}

// ────────────────────────────────────────────────────────────────────────────
// Common envelope (rich events: assistant / user / system / attachment / progress)
// ────────────────────────────────────────────────────────────────────────────

/// Fields shared by claude's "rich" events. Terse events (titles, prompts,
/// permission-mode, etc.) carry only a subset and use their own structs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RichEnvelope {
    pub uuid: String,
    /// `null` on the session's first message.
    pub parent_uuid: Option<String>,
    pub session_id: String,
    pub timestamp: String,
    pub cwd: String,
    pub git_branch: String,
    pub is_sidechain: bool,
    pub user_type: String,
    /// Sometimes `null` on rare events.
    pub entrypoint: Option<String>,
    pub version: String,
    /// Plugin/skill marker; always optional.
    #[serde(default)]
    pub slug: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Assistant
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantEvent {
    #[serde(flatten)]
    pub envelope: RichEnvelope,
    pub message: AssistantMessage,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Set when emitted from a plugin context.
    #[serde(default)]
    pub attribution_plugin: Option<String>,
    /// Set when emitted from a skill context.
    #[serde(default)]
    pub attribution_skill: Option<String>,
    /// Anthropic API error mid-stream.
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub is_api_error_message: Option<bool>,
    #[serde(default)]
    pub api_error_status: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssistantMessage {
    pub id: String,
    pub model: String,
    pub role: String, // always "assistant"
    pub content: Vec<AssistantContentBlock>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    /// Anthropic-API "usage" telemetry.
    pub usage: Usage,
    /// Newer API "container" feature; opaque here.
    #[serde(default)]
    pub container: Option<Value>,
    /// Context-management metadata; opaque.
    #[serde(default)]
    pub context_management: Option<Value>,
    /// Detailed stop information beyond `stop_reason`; opaque.
    #[serde(default)]
    pub stop_details: Option<Value>,
    /// Diagnostics emitted on certain edge cases; opaque.
    #[serde(default)]
    pub diagnostics: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        /// Tool-defined argument schema.
        input: Value,
        /// Sub-agent / nested caller info; opaque.
        #[serde(default)]
        caller: Option<Value>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Detailed cache creation breakdown; opaque.
    #[serde(default)]
    pub cache_creation: Option<Value>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub inference_geo: Option<String>,
    /// Server-tool usage telemetry; opaque.
    #[serde(default)]
    pub server_tool_use: Option<Value>,
    /// When the assistant performed multiple inference iterations on the same
    /// turn, each inner iteration's usage is appended here.
    #[serde(default)]
    pub iterations: Option<Vec<IterationUsage>>,
    #[serde(default)]
    pub speed: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IterationUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation: Option<Value>,
    /// Always "message" for these inner records.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// User
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserEvent {
    #[serde(flatten)]
    pub envelope: RichEnvelope,
    pub message: UserMessage,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<PermissionModeKind>,
    #[serde(default)]
    pub is_meta: Option<bool>,
    #[serde(default)]
    pub is_compact_summary: Option<bool>,
    #[serde(default)]
    pub is_visible_in_transcript_only: Option<bool>,
    #[serde(default)]
    pub source_tool_use_id: Option<String>,
    #[serde(default)]
    pub source_tool_assistant_uuid: Option<String>,
    /// Result of a tool call (for `user` events that wrap tool output).
    #[serde(default)]
    pub tool_use_result: Option<Value>,
    /// MCP-related metadata; opaque.
    #[serde(default)]
    pub mcp_meta: Option<Value>,
    /// Origin of this user message (e.g. injected by a task notification).
    #[serde(default)]
    pub origin: Option<UserMessageOrigin>,
    /// Pasted-image identifiers for use by hooks/skills.
    #[serde(default)]
    pub image_paste_ids: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserMessage {
    pub role: String, // always "user"
    pub content: UserMessageContent,
}

/// User message body: either a bare text string (older / simpler turns)
/// or an array of typed content blocks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum UserMessageContent {
    Text(String),
    Blocks(Vec<UserContentBlock>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentBlock {
    Text {
        text: String,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default)]
        is_error: Option<bool>,
    },
    /// Image attachment (anthropic API `image` block).
    Image {
        source: Value,
    },
    /// Document attachment (anthropic API `document` block).
    Document {
        source: Value,
    },
}

/// `tool_result.content` is either a bare string (28k+ rows) or an array
/// of typed blocks (2k+ rows).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text {
        text: String,
    },
    /// Reference to another tool/turn; payload shape varies.
    ToolReference(Value),
    Image {
        source: Value,
    },
}

// ────────────────────────────────────────────────────────────────────────────
// System
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemEvent {
    #[serde(flatten)]
    pub envelope: RichEnvelope,
    #[serde(flatten)]
    pub kind: SystemKind,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum SystemKind {
    TurnDuration {
        #[serde(rename = "durationMs")]
        duration_ms: u64,
        /// Subagent transcripts (under `projects/*/<sess>/subagents/*.jsonl`)
        /// don't carry `messageCount` — present only on top-level sessions.
        #[serde(rename = "messageCount", default)]
        message_count: Option<u64>,
        #[serde(rename = "isMeta", default)]
        is_meta: Option<bool>,
    },
    StopHookSummary {
        #[serde(rename = "hasOutput")]
        has_output: bool,
        #[serde(rename = "hookCount")]
        hook_count: u32,
        #[serde(rename = "hookErrors")]
        hook_errors: Value,
        #[serde(rename = "hookInfos")]
        hook_infos: Value,
        level: String,
        #[serde(rename = "preventedContinuation")]
        prevented_continuation: bool,
        #[serde(rename = "stopReason")]
        stop_reason: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    AwaySummary {
        content: Value,
        #[serde(rename = "isMeta", default)]
        is_meta: Option<bool>,
    },
    ScheduledTaskFire {
        content: Value,
        #[serde(rename = "isMeta", default)]
        is_meta: Option<bool>,
    },
    LocalCommand {
        content: Value,
        #[serde(rename = "isMeta", default)]
        is_meta: Option<bool>,
        #[serde(default)]
        level: Option<String>,
    },
    Informational {
        content: Value,
        #[serde(rename = "isMeta", default)]
        is_meta: Option<bool>,
        #[serde(default)]
        level: Option<String>,
    },
    BridgeStatus {
        content: Value,
        url: String,
        #[serde(rename = "isMeta", default)]
        is_meta: Option<bool>,
    },
    ApiError {
        error: ApiErrorDetail,
        level: String,
        #[serde(rename = "maxRetries")]
        max_retries: u32,
        #[serde(rename = "retryAttempt")]
        retry_attempt: u32,
        /// Some claude versions emit fractional ms (jittered backoff).
        #[serde(rename = "retryInMs")]
        retry_in_ms: f64,
        /// Optional underlying cause of the API error.
        #[serde(default)]
        cause: Option<Value>,
    },
    CompactBoundary {
        content: Value,
        #[serde(rename = "compactMetadata")]
        compact_metadata: Value,
        #[serde(rename = "logicalParentUuid")]
        logical_parent_uuid: String,
        #[serde(rename = "isMeta", default)]
        is_meta: Option<bool>,
        #[serde(default)]
        level: Option<String>,
    },
}

// ────────────────────────────────────────────────────────────────────────────
// Attachment
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentEvent {
    #[serde(flatten)]
    pub envelope: RichEnvelope,
    pub attachment: AttachmentKind,
}

/// 25 attachment subtypes, observed on the wire.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentKind {
    TaskReminder {
        content: Vec<TodoItem>,
        #[serde(rename = "itemCount")]
        item_count: u32,
    },
    QueuedCommand {
        #[serde(rename = "commandMode")]
        command_mode: String,
        prompt: QueuedCommandPrompt,
        #[serde(rename = "imagePasteIds", default)]
        image_paste_ids: Option<Value>,
        #[serde(rename = "source_uuid", default)]
        source_uuid: Option<String>,
    },
    CommandPermissions {
        #[serde(rename = "allowedTools")]
        allowed_tools: Vec<String>,
    },
    EditedTextFile {
        filename: String,
        snippet: String,
    },
    HookSuccess {
        command: String,
        content: String,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
        #[serde(rename = "exitCode")]
        exit_code: i32,
        #[serde(rename = "hookEvent")]
        hook_event: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        stderr: String,
        stdout: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    DeferredToolsDelta {
        #[serde(rename = "addedLines")]
        added_lines: Value,
        #[serde(rename = "addedNames")]
        added_names: Vec<String>,
        #[serde(rename = "removedNames")]
        removed_names: Vec<String>,
        #[serde(rename = "readdedNames", default)]
        readded_names: Option<Vec<String>>,
        #[serde(rename = "pendingMcpServers", default)]
        pending_mcp_servers: Option<Value>,
    },
    SkillListing {
        content: String,
        #[serde(rename = "isInitial")]
        is_initial: bool,
        #[serde(rename = "skillCount")]
        skill_count: u32,
    },
    HookAdditionalContext {
        /// Hook output is a list of string blobs prepended to context.
        content: Vec<String>,
        #[serde(rename = "hookEvent")]
        hook_event: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    McpInstructionsDelta {
        #[serde(rename = "addedBlocks")]
        added_blocks: Value,
        #[serde(rename = "addedNames")]
        added_names: Vec<String>,
        #[serde(rename = "removedNames")]
        removed_names: Vec<String>,
    },
    NestedMemory {
        content: NestedMemoryContent,
        #[serde(rename = "displayPath")]
        display_path: String,
        path: String,
    },
    File {
        content: FileContent,
        #[serde(rename = "displayPath")]
        display_path: String,
        filename: String,
    },
    DateChange {
        #[serde(rename = "newDate")]
        new_date: String,
    },
    CompactFileReference {
        #[serde(rename = "displayPath")]
        display_path: String,
        filename: String,
    },
    AutoMode {
        #[serde(rename = "reminderType")]
        reminder_type: String,
    },
    InvokedSkills {
        skills: Value,
    },
    UltrathinkEffort {
        #[serde(default)]
        level: Option<String>,
    },
    HookBlockingError {
        #[serde(rename = "blockingError")]
        blocking_error: HookBlockingErrorDetail,
        #[serde(rename = "hookEvent")]
        hook_event: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    Directory {
        content: String,
        #[serde(rename = "displayPath")]
        display_path: String,
        path: String,
    },
    CompanionIntro {
        name: String,
        species: String,
    },
    HookNonBlockingError {
        command: String,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
        #[serde(rename = "exitCode")]
        exit_code: i32,
        #[serde(rename = "hookEvent")]
        hook_event: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        stderr: String,
        stdout: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    TodoReminder {
        content: Vec<TodoItem>,
        #[serde(rename = "itemCount")]
        item_count: u32,
    },
    HookCancelled {
        command: String,
        #[serde(rename = "durationMs")]
        duration_ms: u64,
        #[serde(rename = "hookEvent")]
        hook_event: String,
        #[serde(rename = "hookName")]
        hook_name: String,
        #[serde(rename = "toolUseID")]
        tool_use_id: String,
    },
    DynamicSkill {
        #[serde(rename = "displayPath")]
        display_path: String,
        #[serde(rename = "skillDir")]
        skill_dir: String,
        #[serde(rename = "skillNames")]
        skill_names: Vec<String>,
    },
    AutoModeExit,
    AlreadyReadFile {
        content: FileContent,
        #[serde(rename = "displayPath")]
        display_path: String,
        filename: String,
    },
}

// ────────────────────────────────────────────────────────────────────────────
// Nested attachment content shapes
// ────────────────────────────────────────────────────────────────────────────

/// A todo-list item carried inside a `task_reminder` attachment.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub id: String,
    pub status: String,
    pub subject: String,
    pub description: String,
    #[serde(default)]
    pub active_form: Option<String>,
    #[serde(default)]
    pub blocked_by: Option<Vec<String>>,
    #[serde(default)]
    pub blocks: Option<Vec<String>>,
}

/// `nested_memory.content` is the loaded memory file along with its
/// classification and a flag for in-memory edits.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedMemoryContent {
    pub path: String,
    /// e.g. "Project", "Plugin", ...
    #[serde(rename = "type")]
    pub kind: String,
    pub content: String,
    pub content_differs_from_disk: bool,
}

/// `file.content` and `already_read_file.content` wrap the file body and
/// carry a content-kind discriminator.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileContent {
    Text { file: FileBody },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBody {
    pub file_path: String,
    pub content: String,
    pub num_lines: u32,
    pub start_line: u32,
    pub total_lines: u32,
}

/// Reason a `user` event was synthesized rather than typed by the user.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum UserMessageOrigin {
    /// Notification of a completed background task agent.
    TaskNotification,
}

/// Body of a `system` event with `subtype = "api_error"`.
///
/// Fields are individually optional because the wire shape varies across
/// failure modes (HTTP overload, transport-layer error, internal placeholder).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiErrorDetail {
    /// Anthropic-API error category (e.g. "overloaded_error"); may be `null`.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<u32>,
    /// Headers returned by the upstream service. Older claude versions
    /// emit a `{name: value}` map; newer ones emit just the list of names.
    /// Empty/null when the failure was below the HTTP layer.
    #[serde(default)]
    pub headers: Option<HeaderSet>,
    #[serde(rename = "requestID", default)]
    pub request_id: Option<String>,
    /// Inner anthropic API error payload (when `kind == "overloaded_error"`).
    #[serde(default)]
    pub error: Option<Value>,
    /// Node-level cause (filesystem / network errno) when applicable.
    #[serde(default)]
    pub cause: Option<NodeErrorCause>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeErrorCause {
    pub code: String,
    pub errno: i32,
    pub path: String,
}

/// HTTP response headers, in either of the two shapes claude emits.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum HeaderSet {
    /// `{name: value}` map. Values are usually strings but may be arrays
    /// for repeated headers (e.g. `set-cookie`).
    Map(HashMap<String, Value>),
    /// Bare list of header names (newer claude versions).
    Names(Vec<String>),
}

/// Body of `queued_command.prompt`: either a bare text prompt or an array of
/// content blocks (used when the queued prompt includes an inline image).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum QueuedCommandPrompt {
    Text(String),
    Blocks(Vec<QueuedPromptBlock>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueuedPromptBlock {
    Text { text: String },
    Image { source: Value },
}

/// `{blockingError, command}` payload for `hook_blocking_error` attachment.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookBlockingErrorDetail {
    pub blocking_error: String,
    pub command: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Progress
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProgressEvent {
    #[serde(flatten)]
    pub envelope: RichEnvelope,
    /// Progress payload (e.g. `hook_progress` with hook event/name/command).
    pub data: Value,
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    #[serde(rename = "parentToolUseID")]
    pub parent_tool_use_id: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Permission-mode (also used inline in user events)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionModeKind {
    BypassPermissions,
    Auto,
    Default,
    AcceptEdits,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionModeEvent {
    pub session_id: String,
    pub permission_mode: PermissionModeKind,
}

// ────────────────────────────────────────────────────────────────────────────
// Title types
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTitleEvent {
    pub session_id: String,
    pub ai_title: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomTitleEvent {
    pub session_id: String,
    pub custom_title: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Last-prompt marker
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastPromptEvent {
    pub session_id: String,
    /// Either of these may be absent in some emissions.
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub leaf_uuid: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Queue operations
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueOperationEvent {
    pub session_id: String,
    pub timestamp: String,
    pub operation: QueueOperationKind,
    /// Present for some operations (e.g. enqueue).
    #[serde(default)]
    pub content: Option<Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueOperationKind {
    Enqueue,
    Dequeue,
    Remove,
    PopAll,
}

// ────────────────────────────────────────────────────────────────────────────
// PR link
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrLinkEvent {
    pub session_id: String,
    pub timestamp: String,
    pub pr_number: u64,
    pub pr_repository: String,
    pub pr_url: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Worktree state
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeStateEvent {
    pub session_id: String,
    /// `null` when the session is not running inside a worktree.
    pub worktree_session: Option<WorktreeSession>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSession {
    pub session_id: String,
    pub worktree_branch: String,
    pub worktree_name: String,
    pub worktree_path: String,
    pub original_cwd: String,
    /// Newer schema additions.
    #[serde(default)]
    pub original_branch: Option<String>,
    #[serde(default)]
    pub original_head_commit: Option<String>,
    /// True if attaching to a previously-existing worktree.
    #[serde(default)]
    pub entered_existing: Option<bool>,
}

// ────────────────────────────────────────────────────────────────────────────
// File-history snapshot
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistorySnapshotEvent {
    pub message_id: String,
    pub is_snapshot_update: bool,
    pub snapshot: FileHistorySnapshot,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistorySnapshot {
    pub message_id: String,
    pub timestamp: String,
    /// Map keyed by file path → backup metadata (codex-internal shape).
    pub tracked_file_backups: HashMap<String, Value>,
}

// ────────────────────────────────────────────────────────────────────────────
// Unit tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ClaudeEvent {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("parse failed: {e}\nline: {s}"))
    }

    #[test]
    fn parse_ai_title() {
        let line = r#"{"type":"ai-title","sessionId":"abc","aiTitle":"My Title"}"#;
        let ev = parse(line);
        match ev {
            ClaudeEvent::AiTitle(AiTitleEvent {
                session_id,
                ai_title,
            }) => {
                assert_eq!(session_id, "abc");
                assert_eq!(ai_title, "My Title");
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parse_permission_mode() {
        let line =
            r#"{"type":"permission-mode","sessionId":"abc","permissionMode":"bypassPermissions"}"#;
        let ev = parse(line);
        match ev {
            ClaudeEvent::PermissionMode(PermissionModeEvent {
                session_id,
                permission_mode: PermissionModeKind::BypassPermissions,
            }) => assert_eq!(session_id, "abc"),
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parse_queue_pop_all() {
        let line =
            r#"{"type":"queue-operation","sessionId":"abc","timestamp":"t","operation":"popAll"}"#;
        let ev = parse(line);
        match ev {
            ClaudeEvent::QueueOperation(QueueOperationEvent {
                operation: QueueOperationKind::PopAll,
                ..
            }) => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_type_fails() {
        let line = r#"{"type":"never-seen","sessionId":"x"}"#;
        assert!(serde_json::from_str::<ClaudeEvent>(line).is_err());
    }

    #[test]
    fn parse_unknown_attachment_subtype_fails() {
        let line = r#"{"type":"attachment","uuid":"u","parentUuid":null,"sessionId":"s","timestamp":"t","cwd":"/","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2","attachment":{"type":"never-seen-subtype"}}"#;
        assert!(serde_json::from_str::<ClaudeEvent>(line).is_err());
    }

    #[test]
    fn parse_unknown_system_subtype_fails() {
        let line = r#"{"type":"system","uuid":"u","parentUuid":null,"sessionId":"s","timestamp":"t","cwd":"/","gitBranch":"main","isSidechain":false,"userType":"external","entrypoint":"cli","version":"2","subtype":"never-seen"}"#;
        assert!(serde_json::from_str::<ClaudeEvent>(line).is_err());
    }
}
