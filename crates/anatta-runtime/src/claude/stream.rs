//! Faithful reproduction of Claude Code CLI's stdout streaming protocol.
//!
//! Emitted by `claude --print --output-format stream-json --verbose`.
//! Snake_case envelope, minimal metadata. Distinct from the camelCase
//! disk session JSONL parsed in [`super::history`].
//!
//! Schema mirrored from `@anthropic-ai/claude-agent-sdk@0.2.138`'s
//! `package/sdk.d.ts` (the SDK that wraps the same binary). The
//! Anthropic SSE event union (used inside `stream_event.event`) comes
//! from `@anthropic-ai/sdk@0.95.1`'s
//! `package/resources/beta/messages/messages.d.ts`.
//!
//! Strict tagged-enum semantics: an unknown top-level `type` or
//! `system.subtype` deliberately fails parsing so we notice schema
//! drift.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ────────────────────────────────────────────────────────────────────────────
// Top level
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeStreamEvent {
    Assistant(AssistantMessage),
    User(UserMessage),
    Result(ResultMessage),
    System(SystemMessage),
    StreamEvent(PartialAssistantMessage),
    RateLimitEvent(RateLimitEvent),
    ToolProgress(ToolProgressMessage),
    ToolUseSummary(ToolUseSummaryMessage),
    AuthStatus(AuthStatusMessage),
    PromptSuggestion(PromptSuggestionMessage),
    KeepAlive,
    /// Bidirectional control plane; only relevant for stdin-`stream-json`
    /// input mode. We accept them on stdout too rather than panic.
    ControlRequest(Value),
    ControlResponse(Value),
    ControlCancelRequest(Value),
    /// Rare top-level types referenced from the SDK union but not
    /// commonly emitted on stdout. We accept their payload as Value so
    /// they don't break parsing.
    PostTurnSummary(Value),
    TaskSummary(Value),
    TranscriptMirror(Value),
}

// ────────────────────────────────────────────────────────────────────────────
// Common envelope tail
// ────────────────────────────────────────────────────────────────────────────

/// Identity fields present on (almost) every non-trivial event.
/// Kept separate so each variant can include them via `#[serde(flatten)]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StreamEnvelope {
    pub uuid: String,
    pub session_id: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Assistant
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssistantMessage {
    pub message: BetaMessage,
    pub parent_tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AssistantError>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantError {
    AuthenticationFailed,
    OauthOrgNotAllowed,
    BillingError,
    RateLimit,
    InvalidRequest,
    ServerError,
    Unknown,
    MaxOutputTokens,
}

// ────────────────────────────────────────────────────────────────────────────
// User (covers both SDKUserMessage and SDKUserMessageReplay)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserMessage {
    pub message: BetaMessageParam,
    pub parent_tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_synthetic: Option<bool>,
    /// Tool call results / arbitrary data; intentionally open-shape upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<UserPriority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub should_query: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Optional on `SDKUserMessage`; required on `SDKUserMessageReplay`.
    /// We can't statically discriminate them so both stay optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UserPriority {
    Now,
    Next,
    Later,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MessageOrigin {
    Human,
    Channel {
        server: String,
    },
    Peer {
        from: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    TaskNotification,
    Coordinator,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BetaMessageParam {
    pub role: String,
    pub content: Value,
}

// ────────────────────────────────────────────────────────────────────────────
// Result (success | one of four error subtypes)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ResultMessage {
    Success(ResultSuccess),
    ErrorDuringExecution(ResultError),
    ErrorMaxTurns(ResultError),
    ErrorMaxBudgetUsd(ResultError),
    ErrorMaxStructuredOutputRetries(ResultError),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResultSuccess {
    pub duration_ms: u64,
    pub duration_api_ms: u64,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<u16>,
    pub num_turns: u32,
    pub result: String,
    pub stop_reason: Option<String>,
    pub total_cost_usd: f64,
    pub usage: BetaUsageNonNullable,
    #[serde(default)]
    pub model_usage: HashMap<String, ModelUsage>,
    #[serde(default)]
    pub permission_denials: Vec<PermissionDenial>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_tool_use: Option<DeferredToolUse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<TerminalReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode_state: Option<FastModeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResultError {
    pub duration_ms: u64,
    pub duration_api_ms: u64,
    pub is_error: bool,
    pub num_turns: u32,
    pub stop_reason: Option<String>,
    pub total_cost_usd: f64,
    pub usage: BetaUsageNonNullable,
    #[serde(default)]
    pub model_usage: HashMap<String, ModelUsage>,
    #[serde(default)]
    pub permission_denials: Vec<PermissionDenial>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<TerminalReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode_state: Option<FastModeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeferredToolUse {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    BlockingLimit,
    RapidRefillBreaker,
    PromptTooLong,
    ImageError,
    ModelError,
    AbortedStreaming,
    AbortedTools,
    StopHookPrevented,
    HookStopped,
    ToolDeferred,
    MaxTurns,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FastModeState {
    Off,
    Cooldown,
    On,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PermissionDenial {
    pub tool_name: String,
    pub tool_use_id: String,
    #[serde(default)]
    pub tool_input: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub web_search_requests: u64,
    #[serde(rename = "costUSD")]
    pub cost_usd: f64,
    pub context_window: u64,
    pub max_output_tokens: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// System (with all subtypes)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum SystemMessage {
    Init(SystemInit),
    Status(SystemStatus),
    ApiRetry(SystemApiRetry),
    CompactBoundary(SystemCompactBoundary),
    LocalCommandOutput(SystemLocalCommandOutput),
    HookStarted(SystemHook),
    HookProgress(SystemHookProgress),
    HookResponse(SystemHookResponse),
    PluginInstall(SystemPluginInstall),
    TaskStarted(SystemTaskStarted),
    TaskUpdated(SystemTaskUpdated),
    TaskProgress(SystemTaskProgress),
    TaskNotification(SystemTaskNotification),
    SessionStateChanged(SystemSessionStateChanged),
    Notification(SystemNotification),
    FilesPersisted(SystemFilesPersisted),
    MemoryRecall(SystemMemoryRecall),
    ElicitationComplete(SystemElicitationComplete),
    PermissionDenied(SystemPermissionDenied),
    MirrorError(SystemMirrorError),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemInit {
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(rename = "apiKeySource")]
    pub api_key_source: String,
    #[serde(default)]
    pub betas: Vec<String>,
    pub claude_code_version: String,
    pub cwd: String,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<McpServerStatus>,
    pub model: String,
    #[serde(rename = "permissionMode")]
    pub permission_mode: PermissionMode,
    pub slash_commands: Vec<String>,
    pub output_style: String,
    pub skills: Vec<String>,
    pub plugins: Vec<PluginInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode_state: Option<FastModeState>,
    /// Field added in 2.1.x; present on recent samples but not in the
    /// canonical `sdk.d.ts` types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_paths: Option<Value>,
    /// Field referenced in CHANGELOG 2.1.128 ("plugin_errors"); shape
    /// not pinned in the SDK types yet — keep open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_errors: Option<Value>,
    /// 2.1.x flag: whether usage analytics are disabled for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytics_disabled: Option<bool>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
    DontAsk,
    Auto,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemStatus {
    /// `compacting` | `requesting` | null.
    pub status: Option<String>,
    #[serde(
        default,
        rename = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_error: Option<String>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemApiRetry {
    pub attempt: u32,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    /// `null` for connection errors.
    pub error_status: Option<u16>,
    pub error: AssistantError,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemCompactBoundary {
    pub compact_metadata: CompactMetadata,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompactMetadata {
    pub trigger: CompactTrigger,
    pub pre_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<PreservedSegment>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Manual,
    Auto,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreservedSegment {
    pub head_uuid: String,
    pub anchor_uuid: String,
    pub tail_uuid: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemLocalCommandOutput {
    pub content: String,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemHook {
    pub hook_id: String,
    pub hook_name: String,
    pub hook_event: String,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemHookProgress {
    pub hook_id: String,
    pub hook_name: String,
    pub hook_event: String,
    pub stdout: String,
    pub stderr: String,
    pub output: String,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemHookResponse {
    pub hook_id: String,
    pub hook_name: String,
    pub hook_event: String,
    pub output: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<HookOutcome>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcome {
    Success,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemPluginInstall {
    pub status: PluginInstallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginInstallStatus {
    Started,
    Installed,
    Failed,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemTaskStarted {
    pub task_id: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_transcript: Option<bool>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemTaskProgress {
    pub task_id: String,
    pub description: String,
    pub usage: TaskUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemTaskUpdated {
    pub task_id: String,
    pub patch: Value,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemTaskNotification {
    pub task_id: String,
    pub status: TaskNotificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TaskUsage>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskNotificationStatus {
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskUsage {
    pub total_tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemSessionStateChanged {
    pub state: SessionState,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Running,
    RequiresAction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemNotification {
    pub key: String,
    pub text: String,
    pub priority: NotificationPriority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPriority {
    Low,
    Medium,
    High,
    Immediate,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemFilesPersisted {
    pub files: Vec<PersistedFile>,
    pub failed: Vec<FailedFile>,
    pub processed_at: String,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PersistedFile {
    pub filename: String,
    pub file_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FailedFile {
    pub filename: String,
    pub error: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemMemoryRecall {
    pub mode: MemoryRecallMode,
    pub memories: Vec<RecalledMemory>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRecallMode {
    Select,
    Synthesize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecalledMemory {
    pub path: String,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Personal,
    Team,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemElicitationComplete {
    pub mcp_server_name: String,
    pub elicitation_id: String,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemPermissionDenied {
    pub tool_name: String,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    pub message: String,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemMirrorError {
    pub error: String,
    pub key: MirrorErrorKey,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MirrorErrorKey {
    pub project_key: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// Stream event (wraps Anthropic Beta SSE)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PartialAssistantMessage {
    pub event: BetaRawMessageStreamEvent,
    pub parent_tool_use_id: Option<String>,
    pub uuid: String,
    pub session_id: String,
    /// Time-to-first-token; only on the first event of a stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
}

/// Mirrors `BetaRawMessageStreamEvent` from `@anthropic-ai/sdk` beta types.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BetaRawMessageStreamEvent {
    MessageStart {
        message: BetaMessage,
    },
    MessageDelta {
        delta: MessageDeltaInner,
        usage: BetaMessageDeltaUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_management: Option<ContextManagement>,
    },
    MessageStop,
    ContentBlockStart {
        index: u32,
        content_block: BetaContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: BetaRawContentBlockDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    /// On-wire keep-alive emitted by the API; not in the SDK union but
    /// documented as part of the stream.
    Ping,
    /// On-wire fatal error event.
    Error {
        error: BetaStreamError,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageDeltaInner {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContextManagement {
    #[serde(default)]
    pub applied_edits: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BetaStreamError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Beta Message + Content blocks
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BetaMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub message_type: String,
    pub role: String,
    pub model: String,
    pub content: Vec<BetaContentBlock>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
    pub usage: BetaUsage,
    /// Forwarded from the API, occasionally null on synthetic messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StopDetails {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Container {
    pub id: String,
    pub expires_at: String,
}

/// All content block variants on the Anthropic Beta API.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BetaContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations: Option<Vec<Value>>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<Value>,
    },
    ServerToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<Value>,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<Value>,
    },
    WebFetchToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<Value>,
    },
    AdvisorToolResult {
        tool_use_id: String,
        content: Value,
    },
    CodeExecutionToolResult {
        tool_use_id: String,
        content: Value,
    },
    BashCodeExecutionToolResult {
        tool_use_id: String,
        content: Value,
    },
    TextEditorCodeExecutionToolResult {
        tool_use_id: String,
        content: Value,
    },
    ToolSearchToolResult {
        tool_use_id: String,
        content: Value,
    },
    McpToolUse {
        id: String,
        name: String,
        server_name: String,
        #[serde(default)]
        input: Value,
    },
    McpToolResult {
        tool_use_id: String,
        is_error: bool,
        content: Value,
    },
    ContainerUpload {
        file_id: String,
    },
    /// Beta-only block for context-1m compaction.
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

/// All `delta.type` variants in `content_block_delta`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BetaRawContentBlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    CitationsDelta {
        citation: Value,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    /// Beta-only delta for the `compaction` content block.
    CompactionDelta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
}

// ────────────────────────────────────────────────────────────────────────────
// Usage
// ────────────────────────────────────────────────────────────────────────────

/// `BetaUsage` as it appears in `message_start.message.usage` — most fields
/// are optional in the schema but typically present on the wire.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BetaUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUseUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// `BetaMessageDeltaUsage` is narrower than `BetaUsage` — used in `message_delta`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BetaMessageDeltaUsage {
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUseUsage>,
    /// Newer field appearing in the `result.usage` mirror; per-iteration breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
}

/// `result.usage` is `BetaUsage` with all nullable fields made non-null.
/// In practice some fields may still be missing on synthetic-error results,
/// so we accept the same fields with optional defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BetaUsageNonNullable {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUseUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Value>,
    /// 2.1.x adds a `speed` field on the result-side usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheCreation {
    pub ephemeral_1h_input_tokens: u64,
    pub ephemeral_5m_input_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerToolUseUsage {
    #[serde(default)]
    pub web_fetch_requests: u64,
    #[serde(default)]
    pub web_search_requests: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// Rate limit
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimitEvent {
    pub rate_limit_info: RateLimitInfo,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitInfo {
    #[serde(rename = "status")]
    pub status: RateLimitStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<RateLimitStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_resets_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_disabled_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_using_overage: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surpassed_threshold: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStatus {
    Allowed,
    AllowedWarning,
    Rejected,
}

// ────────────────────────────────────────────────────────────────────────────
// Other top-level
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolProgressMessage {
    pub tool_use_id: String,
    pub tool_name: String,
    pub parent_tool_use_id: Option<String>,
    pub elapsed_time_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolUseSummaryMessage {
    pub summary: String,
    pub preceding_tool_use_ids: Vec<String>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthStatusMessage {
    #[serde(rename = "isAuthenticating")]
    pub is_authenticating: bool,
    pub output: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PromptSuggestionMessage {
    pub suggestion: String,
    pub uuid: String,
    pub session_id: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ClaudeStreamEvent {
        serde_json::from_str(s).unwrap_or_else(|e| panic!("parse failed: {e}\nline: {s}"))
    }

    #[test]
    fn parses_system_init_minimal() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/x","session_id":"s","tools":[],"mcp_servers":[],"model":"m","permissionMode":"default","slash_commands":[],"apiKeySource":"none","claude_code_version":"2.1","output_style":"default","skills":[],"plugins":[],"uuid":"u"}"#;
        match parse(line) {
            ClaudeStreamEvent::System(SystemMessage::Init(_)) => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_system_status() {
        let line = r#"{"type":"system","subtype":"status","status":"requesting","uuid":"u","session_id":"s"}"#;
        match parse(line) {
            ClaudeStreamEvent::System(SystemMessage::Status(s)) => {
                assert_eq!(s.status.as_deref(), Some("requesting"));
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_system_hook_started() {
        let line = r#"{"type":"system","subtype":"hook_started","hook_id":"h","hook_name":"SessionStart:startup","hook_event":"SessionStart","uuid":"u","session_id":"s"}"#;
        match parse(line) {
            ClaudeStreamEvent::System(SystemMessage::HookStarted(_)) => {}
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_result_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"duration_ms":6213,"duration_api_ms":6038,"num_turns":1,"result":"OK","stop_reason":"end_turn","session_id":"s","total_cost_usd":0.089,"usage":{"input_tokens":6,"output_tokens":115,"cache_creation_input_tokens":12336,"cache_read_input_tokens":18924,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":12336,"ephemeral_5m_input_tokens":0},"inference_geo":""},"modelUsage":{},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","uuid":"u"}"#;
        match parse(line) {
            ClaudeStreamEvent::Result(ResultMessage::Success(r)) => {
                assert_eq!(r.result, "OK");
                assert_eq!(r.usage.output_tokens, 115);
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_rate_limit_event() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1778414400,"rateLimitType":"five_hour","overageStatus":"allowed","overageResetsAt":1780272000,"isUsingOverage":false},"uuid":"u","session_id":"s"}"#;
        match parse(line) {
            ClaudeStreamEvent::RateLimitEvent(e) => {
                matches!(e.rate_limit_info.status, RateLimitStatus::Allowed);
                assert_eq!(e.rate_limit_info.resets_at, Some(1778414400));
            }
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_stream_event_message_start() {
        let line = r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-7","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":6,"output_tokens":8}}},"session_id":"s","parent_tool_use_id":null,"uuid":"u","ttft_ms":4133}"#;
        match parse(line) {
            ClaudeStreamEvent::StreamEvent(e) => match e.event {
                BetaRawMessageStreamEvent::MessageStart { message } => {
                    assert_eq!(message.id, "msg_1");
                }
                other => panic!("wrong inner: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_stream_event_content_block_start_thinking() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}},"session_id":"s","parent_tool_use_id":null,"uuid":"u"}"#;
        match parse(line) {
            ClaudeStreamEvent::StreamEvent(e) => match e.event {
                BetaRawMessageStreamEvent::ContentBlockStart { content_block, .. } => {
                    matches!(content_block, BetaContentBlock::Thinking { .. });
                }
                other => panic!("wrong inner: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_stream_event_content_block_delta_signature() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}},"session_id":"s","parent_tool_use_id":null,"uuid":"u"}"#;
        match parse(line) {
            ClaudeStreamEvent::StreamEvent(e) => match e.event {
                BetaRawMessageStreamEvent::ContentBlockDelta { delta, .. } => {
                    matches!(delta, BetaRawContentBlockDelta::SignatureDelta { .. });
                }
                other => panic!("wrong inner: {other:?}"),
            },
            other => panic!("wrong: {other:?}"),
        }
    }

    #[test]
    fn parses_stream_event_content_block_delta_text() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}},"session_id":"s","parent_tool_use_id":null,"uuid":"u"}"#;
        let _ = parse(line);
    }

    #[test]
    fn parses_keep_alive() {
        let line = r#"{"type":"keep_alive"}"#;
        assert!(matches!(parse(line), ClaudeStreamEvent::KeepAlive));
    }

    #[test]
    fn parses_assistant_with_thinking_content() {
        let line = r#"{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-7","content":[{"type":"thinking","thinking":"x","signature":"sig"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":6,"output_tokens":8}},"parent_tool_use_id":null,"session_id":"s","uuid":"u"}"#;
        let _ = parse(line);
    }

    #[test]
    fn parses_assistant_with_tool_use_block() {
        let line = r#"{"type":"assistant","message":{"id":"m","type":"message","role":"assistant","model":"claude-opus-4-7","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"s","uuid":"u"}"#;
        let _ = parse(line);
    }

    #[test]
    fn unknown_top_level_fails() {
        let line = r#"{"type":"never_seen","x":1}"#;
        assert!(serde_json::from_str::<ClaudeStreamEvent>(line).is_err());
    }

    #[test]
    fn unknown_system_subtype_fails() {
        let line = r#"{"type":"system","subtype":"never_seen","uuid":"u","session_id":"s"}"#;
        assert!(serde_json::from_str::<ClaudeStreamEvent>(line).is_err());
    }
}
