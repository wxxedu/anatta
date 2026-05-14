//! End-to-end stress test: parse every line of real session JSONLs,
//! project to AgentEvent via the [`Projector`] trait, and verify the
//! projection never panics. Counts how many events get successfully
//! projected vs filtered out (no semantic correlate in v1).
//!
//! Activated via env vars (skipped if all unset):
//!     ANATTA_CLAUDE_FIXTURE=...        — disk session jsonls
//!     ANATTA_CLAUDE_STREAM_FIXTURE=... — captured stream-json output
//!     ANATTA_CODEX_FIXTURE=...         — rollout jsonls
//!     ANATTA_CODEX_STREAM_FIXTURE=...  — captured exec --json output

use std::fs;
use std::path::{Path, PathBuf};

use anatta_core::{AgentEvent, ProjectionContext, Projector};
use anatta_runtime::{claude, codex};
use chrono::DateTime;

fn fixture_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn collect_jsonl_files(p: &Path) -> Vec<PathBuf> {
    if p.is_file() {
        return vec![p.to_owned()];
    }
    let mut out = Vec::new();
    if p.is_dir() {
        let mut stack = vec![p.to_owned()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    out.push(path);
                }
            }
        }
    }
    out
}

fn ctx() -> ProjectionContext {
    ProjectionContext {
        session_id: "fixture-session".into(),
        received_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    }
}

/// Generic stress runner over any [`Projector`] impl. Reads every
/// jsonl line under the env-var-supplied root, parses to the
/// projector's `Raw` type, and counts emitted vs filtered.
fn stress<P: Projector>(env_var: &str, mut projector: P)
where
    P::Raw: serde::de::DeserializeOwned,
{
    let Some(root) = fixture_path(env_var) else {
        eprintln!("{env_var} unset; skipping");
        return;
    };
    let files = collect_jsonl_files(&root);
    let c = ctx();
    let mut total_lines = 0_usize;
    let mut total_projected = 0_usize;
    let mut empty_projection = 0_usize;
    for file in &files {
        let Ok(content) = fs::read_to_string(file) else {
            continue;
        };
        for line in content.lines() {
            if line.trim().is_empty() || line.starts_with("Error:") || line.starts_with("WARNING:")
            {
                continue;
            }
            total_lines += 1;
            let raw: P::Raw = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(_) => continue, // parser fixture suite owns parse coverage
            };
            let projected: Vec<AgentEvent> = projector.project(&raw, &c);
            if projected.is_empty() {
                empty_projection += 1;
            } else {
                total_projected += projected.len();
            }
        }
    }
    eprintln!(
        "{env_var}: {} files, {} parsed lines → {} agent events ({} raw events filtered)",
        files.len(),
        total_lines,
        total_projected,
        empty_projection
    );
}

#[test]
fn projects_claude_history_without_panic() {
    stress("ANATTA_CLAUDE_FIXTURE", claude::HistoryProjector::new());
}

#[test]
fn projects_claude_stream_without_panic() {
    stress(
        "ANATTA_CLAUDE_STREAM_FIXTURE",
        claude::StreamProjector::new(),
    );
}

#[test]
fn projects_codex_history_without_panic() {
    stress("ANATTA_CODEX_FIXTURE", codex::HistoryProjector::new());
}

#[test]
fn projects_codex_stream_without_panic() {
    stress("ANATTA_CODEX_STREAM_FIXTURE", codex::StreamProjector::new());
}
