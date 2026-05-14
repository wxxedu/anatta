//! Cross-backend runtime version provisioning.
//!
//! Each backend (claude, codex) implements [`BackendDistribution`] to
//! describe how to fetch a specific version's binary for a given platform.
//! The [`install`] function drives the common pipeline:
//!
//!   1. resolve the requested version to a concrete one
//!   2. detect the host platform
//!   3. fetch download metadata (URL + checksum + format)
//!   4. download the artifact
//!   5. verify SHA-256
//!   6. extract / copy the binary into the install directory
//!   7. mark it executable
//!
//! Network and disk operations are async (`tokio`).

mod error;
mod fetch;
mod platform;

#[cfg(test)]
mod tests;

pub use error::DistError;
pub use platform::{Arch, Libc, Os, Platform, detect_platform};

use std::path::{Path, PathBuf};

use async_trait::async_trait;

/// Implemented by each backend (claude, codex) to describe its release channel.
#[async_trait]
pub trait BackendDistribution: Send + Sync {
    /// Backend identifier ("claude" / "codex"). Determines the install
    /// subdirectory under `<install_root>/<name>/<version>/`.
    fn name(&self) -> &'static str;

    /// Resolve a version request to a concrete version string.
    /// E.g. `Latest` → `"0.125.0"`, `Exact("2.1.138")` → `"2.1.138"`.
    async fn resolve_version(&self, request: &VersionRequest) -> Result<String, DistError>;

    /// Look up download metadata for a specific (version, platform) pair.
    async fn download_info(
        &self,
        version: &str,
        platform: Platform,
    ) -> Result<DownloadInfo, DistError>;

    /// Where the final binary should land, relative to `<install_root>/<name>/<version>/`.
    /// Almost always `"bin/<name>"` (or `"bin/<name>.exe"` on Windows).
    fn final_binary_relpath(&self, platform: Platform) -> PathBuf {
        let mut p = PathBuf::from("bin");
        p.push(if platform.os == Os::Windows {
            format!("{}.exe", self.name())
        } else {
            self.name().to_owned()
        });
        p
    }
}

/// What the user asked for.
#[derive(Debug, Clone)]
pub enum VersionRequest {
    /// Whatever the backend says is the newest published version.
    Latest,
    /// The backend's "stable" channel, if it has one. Falls back to Latest.
    Stable,
    /// A specific version literal, e.g. `"2.1.138"`.
    Exact(String),
}

/// Everything we need to fetch one platform's binary for one version.
#[derive(Debug, Clone)]
pub struct DownloadInfo {
    pub url: String,
    /// Expected payload size in bytes (used for progress, not enforced).
    pub size: Option<u64>,
    pub checksum: Checksum,
    pub format: ArchiveFormat,
    /// When `format` is an archive, the relative path of the binary
    /// inside it. Ignored for `Bare`.
    pub binary_in_archive: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Checksum {
    pub algorithm: ChecksumAlgo,
    /// Lowercase hex digest.
    pub hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumAlgo {
    Sha256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// The download IS the binary (no archive wrapping).
    Bare,
    TarGz,
}

/// Result of a successful install.
#[derive(Debug, Clone)]
pub struct InstalledRuntime {
    pub backend: String,
    pub version: String,
    pub binary_path: PathBuf,
}

/// Drive the full install pipeline.
pub async fn install(
    backend: &dyn BackendDistribution,
    request: &VersionRequest,
    install_root: &Path,
) -> Result<InstalledRuntime, DistError> {
    install_with_progress(backend, request, install_root, &|_| {}).await
}

/// Same as [`install`] but with a callback for progress updates.
pub async fn install_with_progress(
    backend: &dyn BackendDistribution,
    request: &VersionRequest,
    install_root: &Path,
    on_progress: &(dyn Fn(Progress) + Sync),
) -> Result<InstalledRuntime, DistError> {
    let version = backend.resolve_version(request).await?;
    on_progress(Progress::ResolvedVersion {
        version: version.clone(),
    });

    let platform = detect_platform()?;
    on_progress(Progress::DetectedPlatform { platform });

    let info = backend.download_info(&version, platform).await?;
    on_progress(Progress::FetchingMetadata {
        url: info.url.clone(),
    });

    let install_dir = install_root.join(backend.name()).join(&version);
    let binary_path = install_dir.join(backend.final_binary_relpath(platform));

    if binary_path.exists() {
        on_progress(Progress::AlreadyInstalled {
            path: binary_path.clone(),
        });
        return Ok(InstalledRuntime {
            backend: backend.name().to_owned(),
            version,
            binary_path,
        });
    }

    let bytes = fetch::download_with_verify(&info, on_progress).await?;
    on_progress(Progress::DownloadComplete { bytes: bytes.len() });

    fetch::install_payload(&info, &bytes, &install_dir, &binary_path).await?;
    on_progress(Progress::Installed {
        path: binary_path.clone(),
    });

    Ok(InstalledRuntime {
        backend: backend.name().to_owned(),
        version,
        binary_path,
    })
}

/// Progress events surfaced during install. CLI converts these to terminal
/// output (spinner, progress bar, final summary).
#[derive(Debug, Clone)]
pub enum Progress {
    ResolvedVersion { version: String },
    DetectedPlatform { platform: Platform },
    FetchingMetadata { url: String },
    DownloadStart { total: Option<u64> },
    DownloadProgress { bytes: u64, total: Option<u64> },
    DownloadComplete { bytes: usize },
    AlreadyInstalled { path: PathBuf },
    Installed { path: PathBuf },
}
