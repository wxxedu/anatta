use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DistError {
    #[error("unsupported host platform: {detail}")]
    UnsupportedPlatform { detail: String },

    #[error("backend `{backend}` does not publish a build for {platform:?}")]
    NoPlatformBuild {
        backend: &'static str,
        platform: super::platform::Platform,
    },

    #[error("backend `{backend}` has no version `{version}` (or no platform-specific build for it)")]
    NoSuchVersion {
        backend: &'static str,
        version: String,
    },

    #[error("network error fetching {url}: {source}")]
    Network { url: String, #[source] source: reqwest::Error },

    #[error("HTTP {status} fetching {url}")]
    HttpStatus { url: String, status: u16 },

    #[error("could not parse {what} from {url}: {detail}")]
    Parse { url: String, what: &'static str, detail: String },

    #[error(
        "checksum mismatch for {url}\n  expected: {expected}\n  actual:   {actual}"
    )]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    #[error("archive extraction failed for {path}: {source}")]
    Archive { path: PathBuf, #[source] source: std::io::Error },

    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },

    #[error("expected binary `{name}` not found inside archive")]
    BinaryNotInArchive { name: String },
}
