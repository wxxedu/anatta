//! End-to-end fixture test: parse every line of a real codex rollout JSONL
//! (or a directory of them) and verify zero failures.
//!
//! Activated via env var so PII-bearing fixtures stay off-repo:
//!
//!     ANATTA_CODEX_FIXTURE=/path/to/file.jsonl  cargo test -p anatta-runtime
//!     ANATTA_CODEX_FIXTURE=/path/to/dir          cargo test -p anatta-runtime
//!
//! Exits with skipped status if env var is unset.

use std::fs;
use std::path::{Path, PathBuf};

use anatta_runtime::codex::history::CodexEvent;

fn fixture_path() -> Option<PathBuf> {
    std::env::var_os("ANATTA_CODEX_FIXTURE").map(PathBuf::from)
}

fn collect_jsonl_files(p: &Path) -> Vec<PathBuf> {
    if p.is_file() {
        return vec![p.to_owned()];
    }
    let mut out = Vec::new();
    if p.is_dir() {
        for entry in walkdir(p) {
            if entry.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(entry);
            }
        }
    }
    out
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn parses_real_codex_sessions() {
    let Some(root) = fixture_path() else {
        eprintln!("ANATTA_CODEX_FIXTURE unset; skipping");
        return;
    };
    let files = collect_jsonl_files(&root);
    assert!(
        !files.is_empty(),
        "no .jsonl files found under {}",
        root.display()
    );

    let mut total_lines = 0_usize;
    let mut failures: Vec<(PathBuf, usize, String, String)> = Vec::new();

    for file in &files {
        let Ok(content) = fs::read_to_string(file) else { continue };
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            total_lines += 1;
            if let Err(e) = serde_json::from_str::<CodexEvent>(line) {
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
        "codex fixtures: {} files, {} lines, {} failures",
        files.len(),
        total_lines,
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
    let limit = 200;
    if s.len() <= limit {
        return s.to_owned();
    }
    let mut end = limit;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
