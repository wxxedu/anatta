//! Codex distribution channel: GitHub releases.
//!
//! Asset naming follows Rust target triples, e.g.
//! `codex-aarch64-apple-darwin.tar.gz`. The tarball contains a single
//! file named `codex-<triple>` which we rename to `codex` on install.
//!
//! GitHub does not publish per-asset checksums, so anatta maintains its
//! own embedded `(version, platform) → sha256` table. New versions are
//! added by computing the sha256 manually and PR'ing them in.

use async_trait::async_trait;

use crate::distribution::{
    ArchiveFormat, BackendDistribution, Checksum, ChecksumAlgo, DistError, DownloadInfo, Platform,
    VersionRequest,
};

pub struct CodexDistribution;

impl CodexDistribution {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexDistribution {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BackendDistribution for CodexDistribution {
    fn name(&self) -> &'static str {
        "codex"
    }

    async fn resolve_version(&self, request: &VersionRequest) -> Result<String, DistError> {
        match request {
            VersionRequest::Latest | VersionRequest::Stable => {
                // Pin to the newest version anatta has tested. Promotion to a
                // newer version happens by adding it to the embedded table
                // below and shipping a new anatta release.
                Ok(KNOWN_VERSIONS
                    .iter()
                    .map(|v| v.version)
                    .max_by(|a, b| compare_semver(a, b))
                    .ok_or_else(|| DistError::NoSuchVersion {
                        backend: "codex",
                        version: "<latest>".into(),
                    })?
                    .to_owned())
            }
            VersionRequest::Exact(v) => {
                if KNOWN_VERSIONS.iter().any(|kv| kv.version == v) {
                    Ok(v.clone())
                } else {
                    Err(DistError::NoSuchVersion {
                        backend: "codex",
                        version: v.clone(),
                    })
                }
            }
        }
    }

    async fn download_info(
        &self,
        version: &str,
        platform: Platform,
    ) -> Result<DownloadInfo, DistError> {
        let triple = rust_target_triple(platform).ok_or(DistError::NoPlatformBuild {
            backend: "codex",
            platform,
        })?;

        let kv = KNOWN_VERSIONS
            .iter()
            .find(|kv| kv.version == version)
            .ok_or_else(|| DistError::NoSuchVersion {
                backend: "codex",
                version: version.to_owned(),
            })?;

        let entry = kv.platform_for(triple).ok_or(DistError::NoPlatformBuild {
            backend: "codex",
            platform,
        })?;

        let url = format!(
            "https://github.com/openai/codex/releases/download/rust-v{version}/codex-{triple}.tar.gz",
        );

        Ok(DownloadInfo {
            url,
            size: Some(entry.size),
            checksum: Checksum {
                algorithm: ChecksumAlgo::Sha256,
                hex: entry.sha256.to_owned(),
            },
            format: ArchiveFormat::TarGz,
            binary_in_archive: Some(format!("codex-{triple}")),
        })
    }
}

/// Map an anatta `Platform` to the Rust target triple codex publishes.
/// Returns `None` if codex doesn't ship a build for that platform.
fn rust_target_triple(p: Platform) -> Option<&'static str> {
    use crate::distribution::{Arch, Libc, Os};
    Some(match (p.os, p.arch, p.libc) {
        (Os::Macos, Arch::Aarch64, _) => "aarch64-apple-darwin",
        (Os::Macos, Arch::X86_64, _) => "x86_64-apple-darwin",
        (Os::Linux, Arch::Aarch64, Libc::Glibc) => "aarch64-unknown-linux-gnu",
        (Os::Linux, Arch::X86_64, Libc::Glibc) => "x86_64-unknown-linux-gnu",
        (Os::Linux, Arch::Aarch64, Libc::Musl) => "aarch64-unknown-linux-musl",
        (Os::Linux, Arch::X86_64, Libc::Musl) => "x86_64-unknown-linux-musl",
        // Windows codex assets have `.exe.tar.gz` etc; not yet wired.
        (Os::Windows, _, _) => return None,
        _ => return None,
    })
}

/// Naive semver comparator: split on `.`, compare numerically.
fn compare_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts: Vec<u32> = a.split('.').filter_map(|p| p.parse().ok()).collect();
    let b_parts: Vec<u32> = b.split('.').filter_map(|p| p.parse().ok()).collect();
    a_parts.cmp(&b_parts)
}

// ────────────────────────────────────────────────────────────────────────────
// Embedded known-version table.
//
// Adding a new version:
//   1. Find the asset URL on GitHub (rust-v<version>/codex-<triple>.tar.gz).
//   2. `curl -fsSL <url> -o /tmp/x.tar.gz && shasum -a 256 /tmp/x.tar.gz`
//   3. Append a new `KnownVersion` entry below.
//   4. Bump anatta version, ship.
// ────────────────────────────────────────────────────────────────────────────

struct KnownVersion {
    version: &'static str,
    platforms: &'static [PlatformEntry],
}

impl KnownVersion {
    fn platform_for(&self, triple: &str) -> Option<&PlatformEntry> {
        self.platforms.iter().find(|p| p.triple == triple)
    }
}

struct PlatformEntry {
    triple: &'static str,
    sha256: &'static str,
    size: u64,
}

const KNOWN_VERSIONS: &[KnownVersion] = &[KnownVersion {
    version: "0.125.0",
    platforms: &[PlatformEntry {
        triple: "aarch64-apple-darwin",
        sha256: "6a926dc0cb9639d349b62beda1907c53cb1349709e7dc9cfc53268f438cb749f",
        size: 74_665_914,
    }],
}];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distribution::{Arch, Libc, Os};

    #[tokio::test]
    async fn resolve_latest_picks_max_version() {
        let v = CodexDistribution
            .resolve_version(&VersionRequest::Latest)
            .await
            .unwrap();
        assert_eq!(v, "0.125.0");
    }

    #[tokio::test]
    async fn resolve_exact_known() {
        let v = CodexDistribution
            .resolve_version(&VersionRequest::Exact("0.125.0".into()))
            .await
            .unwrap();
        assert_eq!(v, "0.125.0");
    }

    #[tokio::test]
    async fn resolve_exact_unknown_errors() {
        let err = CodexDistribution
            .resolve_version(&VersionRequest::Exact("9.9.9".into()))
            .await
            .unwrap_err();
        assert!(matches!(err, DistError::NoSuchVersion { .. }));
    }

    #[tokio::test]
    async fn download_info_darwin_arm64() {
        let p = Platform {
            os: Os::Macos,
            arch: Arch::Aarch64,
            libc: Libc::None,
        };
        let info = CodexDistribution.download_info("0.125.0", p).await.unwrap();
        assert_eq!(
            info.url,
            "https://github.com/openai/codex/releases/download/rust-v0.125.0/codex-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            info.checksum.hex,
            "6a926dc0cb9639d349b62beda1907c53cb1349709e7dc9cfc53268f438cb749f"
        );
        assert_eq!(info.size, Some(74_665_914));
        assert!(matches!(info.format, ArchiveFormat::TarGz));
        assert_eq!(
            info.binary_in_archive.as_deref(),
            Some("codex-aarch64-apple-darwin")
        );
    }

    #[tokio::test]
    async fn download_info_unsupported_platform() {
        let p = Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
            libc: Libc::None,
        };
        let err = CodexDistribution
            .download_info("0.125.0", p)
            .await
            .unwrap_err();
        assert!(matches!(err, DistError::NoPlatformBuild { .. }));
    }
}
