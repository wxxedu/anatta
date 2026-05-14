//! Conversation-level render / absorb cycle.
//!
//! `anatta` maintains the canonical record of a conversation as a sequence
//! of per-segment `events.jsonl` files in a central anatta-owned directory
//! (`<anatta_home>/conversations/<conv-ulid>/segments/<segment-ulid>/`).
//!
//! Between the central store and the CLI subprocess sits a derived working
//! file under the profile's `projects/` (in tier 1, this is shared via
//! symlink; the design treats it as ephemeral working area regardless).
//!
//! - **`render`**: read central segments, apply per-segment policy, write
//!   the working file (atomic via tmp + rename). Used at session start
//!   and on profile swap.
//!
//! - **`absorb`**: tail the working file from a stored offset, append
//!   new bytes to the active segment's central events.jsonl. Used after
//!   each CLI exit. Crash-idempotent.
//!
//! The two functions are pure file IO + the `claude::sanitize::strip_reasoning`
//! filter. No DB. No store. The caller (anatta-cli) reads DB rows, computes
//! the paths, and dispatches.

pub mod absorb;
pub mod paths;
pub mod render;
pub mod render_v2;
pub mod sidecar;

pub use sidecar::{copy_dir_recursive, sync_sidecar_one_way};

pub use absorb::{AbsorbError, AbsorbInput, AbsorbOutcome, absorb_after_turn};
pub use paths::{encode_cwd, working_jsonl_path, working_sidecar_dir};
pub use render::{PriorSegmentInput, RenderError, RenderOutcome, render_into_working};
