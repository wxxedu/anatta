//! End-to-end install test for Claude Code.
//!
//! Downloads ~200 MB from `downloads.claude.ai`, verifies SHA-256 against
//! Anthropic's manifest, places the binary in a tempdir, and confirms
//! `claude --version` reports the expected version.
//!
//! Marked `#[ignore]` due to download size. Run explicitly:
//!
//!     cargo test -p anatta-runtime --features installer --test claude_install -- --ignored --nocapture

#![cfg(feature = "installer")]

use anatta_runtime::claude::ClaudeDistribution;
use anatta_runtime::distribution::{VersionRequest, install};

#[tokio::test]
#[ignore = "downloads ~200 MB; run with --ignored"]
async fn install_claude_latest_works_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let install_root = tmp.path();

    let installed = install(
        &ClaudeDistribution::new(),
        &VersionRequest::Latest,
        install_root,
    )
    .await
    .expect("install pipeline");

    assert_eq!(installed.backend, "claude");
    assert!(installed.binary_path.exists(), "binary not on disk");
    assert!(installed.binary_path.ends_with("bin/claude"));

    // Sanity: the installed binary actually runs and reports the version.
    let output = std::process::Command::new(&installed.binary_path)
        .arg("--version")
        .output()
        .expect("spawn claude");
    assert!(
        output.status.success(),
        "claude --version exit: {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&installed.version),
        "expected version {} in output, got: {stdout}",
        installed.version
    );
}

#[tokio::test]
#[ignore = "downloads ~200 MB; run with --ignored"]
async fn install_claude_specific_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let install_root = tmp.path();

    let installed = install(
        &ClaudeDistribution::new(),
        &VersionRequest::Exact("2.1.138".into()),
        install_root,
    )
    .await
    .expect("install pipeline");

    assert_eq!(installed.version, "2.1.138");

    let output = std::process::Command::new(&installed.binary_path)
        .arg("--version")
        .output()
        .expect("spawn claude");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("2.1.138"), "got: {stdout}");
}
