//! Sidecar directory mirroring helpers.
//!
//! Claude's session sidecar (`<profile>/projects/<encoded_cwd>/<session_uuid>/`)
//! contains:
//!   - `subagents/` — Task-tool sub-agent transcripts (JSONL + meta.json)
//!   - `tool-results/` — large tool output offloaded as separate files
//!
//! Render copies the whole sidecar from central → working (all-or-nothing
//! via tmp + rename). Absorb mirrors newly-appeared files from working →
//! central (no deletions; sidecar is append-only from the CLI's
//! perspective).
//!
//! Tier 1: sub-agent transcripts inside `subagents/*.jsonl` are NOT
//! sanitized for thinking blocks. Tier 1.x adds that.

use std::fs;
use std::io;
use std::path::Path;

/// Recursively copy `src` to `dst`. If `dst` already exists, the contents
/// are merged (existing dst files are overwritten by src). Used by render
/// after the tmp+rename atomic pattern.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ty.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
        // Symlinks etc.: ignore (claude doesn't produce them).
    }
    Ok(())
}

/// Mirror newly-appeared files from `working` → `central`. For each file
/// in `working` that has no counterpart in `central` (or has a different
/// size), copy it over. Never deletes from `central`; sidecar files are
/// append-only from the CLI's POV.
///
/// Returns an error if a same-named file in working has a SMALLER size
/// than its central counterpart — that would indicate corruption /
/// truncation, which is an anomaly worth surfacing.
pub fn sync_sidecar_one_way(working: &Path, central: &Path) -> io::Result<()> {
    fs::create_dir_all(central)?;
    if !working.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(working)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = central.join(entry.file_name());
        if ty.is_dir() {
            sync_sidecar_one_way(&src_path, &dst_path)?;
        } else if ty.is_file() {
            let src_size = entry.metadata()?.len();
            if let Ok(dst_meta) = fs::metadata(&dst_path) {
                let dst_size = dst_meta.len();
                if dst_size == src_size {
                    continue; // already mirrored
                }
                if dst_size > src_size {
                    return Err(io::Error::other(format!(
                        "sidecar file shrunk in working area: {} ({} → {} bytes)",
                        src_path.display(),
                        dst_size,
                        src_size,
                    )));
                }
                // src grew; overwrite central
            }
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn copy_dir_recursive_creates_layout() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("subagents")).unwrap();
        fs::create_dir_all(src.join("tool-results")).unwrap();
        fs::write(src.join("subagents/agent-1.jsonl"), b"hello").unwrap();
        fs::write(src.join("tool-results/out.txt"), b"world").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert!(dst.join("subagents/agent-1.jsonl").exists());
        assert!(dst.join("tool-results/out.txt").exists());
        assert_eq!(
            fs::read(dst.join("subagents/agent-1.jsonl")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn sync_sidecar_mirrors_new_files() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working");
        let central = tmp.path().join("central");
        fs::create_dir_all(working.join("subagents")).unwrap();
        fs::write(working.join("subagents/a.jsonl"), b"line1\nline2\n").unwrap();

        sync_sidecar_one_way(&working, &central).unwrap();
        assert!(central.join("subagents/a.jsonl").exists());

        // Append more content to working file
        fs::write(working.join("subagents/a.jsonl"), b"line1\nline2\nline3\n").unwrap();
        sync_sidecar_one_way(&working, &central).unwrap();
        assert_eq!(
            fs::read(central.join("subagents/a.jsonl")).unwrap(),
            b"line1\nline2\nline3\n"
        );
    }

    #[test]
    fn sync_sidecar_errors_on_shrink() {
        let tmp = TempDir::new().unwrap();
        let working = tmp.path().join("working");
        let central = tmp.path().join("central");
        fs::create_dir_all(working.join("subagents")).unwrap();
        fs::create_dir_all(central.join("subagents")).unwrap();
        fs::write(central.join("subagents/a.jsonl"), b"big big big content").unwrap();
        fs::write(working.join("subagents/a.jsonl"), b"short").unwrap();

        let err = sync_sidecar_one_way(&working, &central).unwrap_err();
        assert!(err.to_string().contains("shrunk"));
    }
}
