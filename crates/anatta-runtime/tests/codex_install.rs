//! End-to-end install test: actually downloads codex 0.125.0 from GitHub,
//! verifies the SHA-256, extracts the binary into a tempdir, and confirms
//! the installed binary reports the expected `--version`.
//!
//! Marked `#[ignore]` because it requires network and ~75 MB download.
//! Run explicitly:
//!
//!     cargo test -p anatta-runtime --features installer --test codex_install -- --ignored --nocapture

#![cfg(feature = "installer")]

use anatta_runtime::codex::CodexDistribution;
use anatta_runtime::distribution::{install, VersionRequest};

#[tokio::test]
#[ignore = "downloads ~75 MB from GitHub; run with --ignored"]
async fn install_codex_0_125_0_works_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let install_root = tmp.path();

    let installed = install(
        &CodexDistribution::new(),
        &VersionRequest::Exact("0.125.0".into()),
        install_root,
    )
    .await
    .expect("install pipeline");

    assert_eq!(installed.backend, "codex");
    assert_eq!(installed.version, "0.125.0");
    assert!(installed.binary_path.exists(), "binary not on disk");
    assert!(installed.binary_path.ends_with("bin/codex"));

    // Sanity: the installed binary actually runs and reports the version.
    let output = std::process::Command::new(&installed.binary_path)
        .arg("--version")
        .output()
        .expect("spawn codex");
    assert!(output.status.success(), "codex --version exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.125.0"),
        "unexpected version output: {stdout}"
    );
}

#[tokio::test]
#[ignore = "downloads ~75 MB; run with --ignored"]
async fn second_install_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let install_root = tmp.path();

    let first = install(
        &CodexDistribution::new(),
        &VersionRequest::Exact("0.125.0".into()),
        install_root,
    )
    .await
    .expect("first install");

    let mtime_first = std::fs::metadata(&first.binary_path)
        .expect("stat after first install")
        .modified()
        .expect("mtime");

    // Sleep 1s so any actual rewrite would change mtime.
    std::thread::sleep(std::time::Duration::from_secs(1));

    let second = install(
        &CodexDistribution::new(),
        &VersionRequest::Exact("0.125.0".into()),
        install_root,
    )
    .await
    .expect("second install");

    assert_eq!(first.binary_path, second.binary_path);
    let mtime_second = std::fs::metadata(&second.binary_path)
        .expect("stat after second install")
        .modified()
        .expect("mtime");
    assert_eq!(
        mtime_first, mtime_second,
        "second install should have skipped (idempotent)"
    );
}
