//! End-to-end fixture test: parse every line of one or more real Claude
//! Code session JSONLs and verify zero failures.
//!
//! Activated via env var so PII-bearing fixtures stay off-repo:
//!
//!     ANATTA_CLAUDE_FIXTURE=/path/to/file.jsonl  cargo test -p anatta-runtime
//!     ANATTA_CLAUDE_FIXTURE=/path/to/dir          cargo test -p anatta-runtime
//!
//! Skipped when env var is unset.

use std::fs;
use std::path::{Path, PathBuf};

use anatta_runtime::claude::history::ClaudeEvent;

fn fixture_path() -> Option<PathBuf> {
    std::env::var_os("ANATTA_CLAUDE_FIXTURE").map(PathBuf::from)
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

#[test]
fn parses_real_claude_sessions() {
    let Some(root) = fixture_path() else {
        eprintln!("ANATTA_CLAUDE_FIXTURE unset; skipping");
        return;
    };
    let files = collect_jsonl_files(&root);
    assert!(
        !files.is_empty(),
        "no .jsonl files found under {}",
        root.display()
    );

    let mut total = 0_usize;
    let mut failures: Vec<(PathBuf, usize, String, String)> = Vec::new();

    for file in &files {
        let Ok(content) = fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            if let Err(e) = serde_json::from_str::<ClaudeEvent>(line) {
                failures.push((file.clone(), i + 1, e.to_string(), line.to_owned()));
                if failures.len() >= 25 {
                    break;
                }
            }
        }
        if failures.len() >= 25 {
            break;
        }
    }

    eprintln!(
        "claude fixtures: {} files, {} lines, {} failures",
        files.len(),
        total,
        failures.len()
    );

    if !failures.is_empty() {
        for (path, line, err, raw) in failures.iter().take(10) {
            eprintln!(
                "\n--- {}:{} ---\n  err: {}\n  raw: {}",
                path.display(),
                line,
                err,
                preview(raw)
            );
        }
        panic!("{} parse failures (showing up to 10)", failures.len());
    }
}

fn preview(s: &str) -> String {
    let limit = 220;
    if s.len() <= limit {
        return s.to_owned();
    }
    let mut end = limit;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
