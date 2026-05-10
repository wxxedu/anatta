//! Claude Code distribution channel: Anthropic's downloads CDN.
//!
//! Anthropic publishes:
//!   * `https://downloads.claude.ai/claude-code-releases/latest` — plain
//!     text body containing the latest version (e.g. `"2.1.138"`).
//!   * `https://downloads.claude.ai/claude-code-releases/stable`  — same
//!     shape, points to the stable channel.
//!   * `https://downloads.claude.ai/claude-code-releases/<v>/manifest.json`
//!     — per-platform `{binary, checksum, size}` entries.
//!   * `https://downloads.claude.ai/claude-code-releases/<v>/<platform>/<binary>`
//!     — the actual native binary (bare, not archived).
//!
//! Because Anthropic publishes their own SHA-256 checksums per platform,
//! we don't maintain an embedded version table — we just fetch and trust.

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::distribution::{
    ArchiveFormat, BackendDistribution, Checksum, ChecksumAlgo, DistError, DownloadInfo, Platform,
    VersionRequest,
};

const BASE_URL: &str = "https://downloads.claude.ai/claude-code-releases";

pub struct ClaudeDistribution {
    base_url: String,
}

impl ClaudeDistribution {
    pub fn new() -> Self {
        Self {
            base_url: BASE_URL.into(),
        }
    }

    /// Override the base URL. Used by tests against a local mock.
    #[doc(hidden)]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for ClaudeDistribution {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BackendDistribution for ClaudeDistribution {
    fn name(&self) -> &'static str {
        "claude"
    }

    async fn resolve_version(&self, request: &VersionRequest) -> Result<String, DistError> {
        let pointer = match request {
            VersionRequest::Latest => "latest",
            VersionRequest::Stable => "stable",
            VersionRequest::Exact(v) => return Ok(v.clone()),
        };
        let url = format!("{}/{}", self.base_url, pointer);
        let body = http_get_text(&url).await?;
        Ok(body.trim().to_owned())
    }

    async fn download_info(
        &self,
        version: &str,
        platform: Platform,
    ) -> Result<DownloadInfo, DistError> {
        let platform_str = claude_platform_string(platform).ok_or(DistError::NoPlatformBuild {
            backend: "claude",
            platform,
        })?;

        let manifest_url = format!("{}/{}/manifest.json", self.base_url, version);
        let manifest: Manifest = http_get_json(&manifest_url).await?;

        if manifest.version != version {
            return Err(DistError::Parse {
                url: manifest_url.clone(),
                what: "manifest.version",
                detail: format!(
                    "expected {version}, got {}",
                    manifest.version
                ),
            });
        }

        let entry = manifest.platforms.get(platform_str).ok_or(DistError::NoPlatformBuild {
            backend: "claude",
            platform,
        })?;

        let url = format!(
            "{}/{}/{}/{}",
            self.base_url, version, platform_str, entry.binary
        );

        Ok(DownloadInfo {
            url,
            size: Some(entry.size),
            checksum: Checksum {
                algorithm: ChecksumAlgo::Sha256,
                hex: entry.checksum.clone(),
            },
            format: ArchiveFormat::Bare,
            binary_in_archive: None,
        })
    }
}

/// Map an anatta `Platform` to the string Anthropic uses in URLs and
/// manifest keys (`darwin-arm64`, `linux-x64-musl`, ...).
fn claude_platform_string(p: Platform) -> Option<&'static str> {
    use crate::distribution::{Arch, Libc, Os};
    Some(match (p.os, p.arch, p.libc) {
        (Os::Macos, Arch::Aarch64, _) => "darwin-arm64",
        (Os::Macos, Arch::X86_64, _) => "darwin-x64",
        (Os::Linux, Arch::Aarch64, Libc::Musl) => "linux-arm64-musl",
        (Os::Linux, Arch::X86_64, Libc::Musl) => "linux-x64-musl",
        (Os::Linux, Arch::Aarch64, _) => "linux-arm64",
        (Os::Linux, Arch::X86_64, _) => "linux-x64",
        (Os::Windows, Arch::Aarch64, _) => "win32-arm64",
        (Os::Windows, Arch::X86_64, _) => "win32-x64",
    })
}

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    platforms: BTreeMap<String, PlatformEntry>,
    // Other fields like `commit`, `buildDate` exist but we don't need them.
}

#[derive(Debug, Deserialize)]
struct PlatformEntry {
    binary: String,
    checksum: String,
    size: u64,
}

async fn http_get_text(url: &str) -> Result<String, DistError> {
    let resp = client(url)?
        .get(url)
        .send()
        .await
        .map_err(|e| DistError::Network {
            url: url.to_owned(),
            source: e,
        })?;
    if !resp.status().is_success() {
        return Err(DistError::HttpStatus {
            url: url.to_owned(),
            status: resp.status().as_u16(),
        });
    }
    resp.text().await.map_err(|e| DistError::Network {
        url: url.to_owned(),
        source: e,
    })
}

async fn http_get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, DistError> {
    let resp = client(url)?
        .get(url)
        .send()
        .await
        .map_err(|e| DistError::Network {
            url: url.to_owned(),
            source: e,
        })?;
    if !resp.status().is_success() {
        return Err(DistError::HttpStatus {
            url: url.to_owned(),
            status: resp.status().as_u16(),
        });
    }
    resp.json::<T>().await.map_err(|e| DistError::Parse {
        url: url.to_owned(),
        what: "json",
        detail: e.to_string(),
    })
}

fn client(url: &str) -> Result<reqwest::Client, DistError> {
    reqwest::Client::builder()
        .user_agent(concat!("anatta/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| DistError::Network {
            url: url.to_owned(),
            source: e,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distribution::{Arch, Libc, Os};

    #[test]
    fn platform_string_mapping() {
        let p = Platform { os: Os::Macos, arch: Arch::Aarch64, libc: Libc::None };
        assert_eq!(claude_platform_string(p), Some("darwin-arm64"));

        let p = Platform { os: Os::Linux, arch: Arch::X86_64, libc: Libc::Musl };
        assert_eq!(claude_platform_string(p), Some("linux-x64-musl"));

        let p = Platform { os: Os::Linux, arch: Arch::X86_64, libc: Libc::Glibc };
        assert_eq!(claude_platform_string(p), Some("linux-x64"));

        let p = Platform { os: Os::Windows, arch: Arch::X86_64, libc: Libc::None };
        assert_eq!(claude_platform_string(p), Some("win32-x64"));
    }

    #[tokio::test]
    async fn resolve_exact_doesnt_hit_network() {
        // Use a bogus base URL — Exact must short-circuit.
        let dist = ClaudeDistribution::with_base_url("http://127.0.0.1:1");
        let v = dist
            .resolve_version(&VersionRequest::Exact("2.1.138".into()))
            .await
            .unwrap();
        assert_eq!(v, "2.1.138");
    }
}
