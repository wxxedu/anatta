//! Download + verify + install pipeline shared by all backends.

use std::io::Read;
use std::path::Path;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::error::DistError;
use super::{ArchiveFormat, Checksum, ChecksumAlgo, DownloadInfo, Progress};

/// Stream a URL into memory while reporting progress, then verify the
/// digest. Returns the full payload bytes on success.
///
/// We accumulate in memory rather than streaming to disk because the
/// downstream step (extract / install) is small and benefits from
/// having the whole blob available; current backends top out around
/// 200 MB which is comfortable.
pub(super) async fn download_with_verify(
    info: &DownloadInfo,
    on_progress: &(dyn Fn(Progress) + Sync),
) -> Result<Vec<u8>, DistError> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("anatta/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| DistError::Network {
            url: info.url.clone(),
            source: e,
        })?;

    let resp = client.get(&info.url).send().await.map_err(|e| DistError::Network {
        url: info.url.clone(),
        source: e,
    })?;

    if !resp.status().is_success() {
        return Err(DistError::HttpStatus {
            url: info.url.clone(),
            status: resp.status().as_u16(),
        });
    }

    let total = resp.content_length().or(info.size);
    on_progress(Progress::DownloadStart { total });

    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DistError::Network {
            url: info.url.clone(),
            source: e,
        })?;
        downloaded += chunk.len() as u64;
        bytes.extend_from_slice(&chunk);
        on_progress(Progress::DownloadProgress {
            bytes: downloaded,
            total,
        });
    }

    verify_checksum(&info.url, &info.checksum, &bytes)?;
    Ok(bytes)
}

fn verify_checksum(url: &str, expected: &Checksum, bytes: &[u8]) -> Result<(), DistError> {
    let actual = match expected.algorithm {
        ChecksumAlgo::Sha256 => {
            let mut h = Sha256::new();
            h.update(bytes);
            hex::encode(h.finalize())
        }
    };
    if !actual.eq_ignore_ascii_case(&expected.hex) {
        return Err(DistError::ChecksumMismatch {
            url: url.to_owned(),
            expected: expected.hex.clone(),
            actual,
        });
    }
    Ok(())
}

/// Place the verified payload into its final location (extracting if needed).
pub(super) async fn install_payload(
    info: &DownloadInfo,
    bytes: &[u8],
    install_dir: &Path,
    binary_path: &Path,
) -> Result<(), DistError> {
    if let Some(parent) = binary_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| DistError::Io {
            path: parent.to_owned(),
            source: e,
        })?;
    }

    match info.format {
        ArchiveFormat::Bare => {
            write_executable(binary_path, bytes).await?;
        }
        ArchiveFormat::TarGz => {
            extract_tar_gz(bytes, info, install_dir, binary_path).await?;
        }
    }

    Ok(())
}

async fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), DistError> {
    let mut f = tokio::fs::File::create(path).await.map_err(|e| DistError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    f.write_all(bytes).await.map_err(|e| DistError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    f.flush().await.map_err(|e| DistError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    drop(f);

    chmod_executable(path)?;
    Ok(())
}

#[cfg(unix)]
fn chmod_executable(path: &Path) -> Result<(), DistError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| DistError::Io {
        path: path.to_owned(),
        source: e,
    })
}

#[cfg(not(unix))]
fn chmod_executable(_path: &Path) -> Result<(), DistError> {
    // Windows binaries are executable based on extension; nothing to do.
    Ok(())
}

async fn extract_tar_gz(
    bytes: &[u8],
    info: &DownloadInfo,
    install_dir: &Path,
    binary_path: &Path,
) -> Result<(), DistError> {
    // tar+flate2 are sync; extract on a blocking task.
    let bytes = bytes.to_vec();
    let info = info.clone();
    let install_dir = install_dir.to_owned();
    let binary_path = binary_path.to_owned();
    let install_dir_for_err = install_dir.clone();

    tokio::task::spawn_blocking(move || {
        extract_tar_gz_blocking(&bytes, &info, &install_dir, &binary_path)
    })
    .await
    .map_err(|e| DistError::Archive {
        path: install_dir_for_err,
        source: std::io::Error::other(e),
    })?
}

fn extract_tar_gz_blocking(
    bytes: &[u8],
    info: &DownloadInfo,
    _install_dir: &Path,
    binary_path: &Path,
) -> Result<(), DistError> {
    let want_inner = info.binary_in_archive.as_deref().ok_or_else(|| {
        DistError::Parse {
            url: info.url.clone(),
            what: "binary_in_archive",
            detail: "TarGz format requires binary_in_archive to be set".into(),
        }
    })?;

    let tar = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(tar);

    for entry in archive.entries().map_err(|e| DistError::Archive {
        path: binary_path.to_owned(),
        source: e,
    })? {
        let mut entry = entry.map_err(|e| DistError::Archive {
            path: binary_path.to_owned(),
            source: e,
        })?;
        let path_in_tar = entry.path().map_err(|e| DistError::Archive {
            path: binary_path.to_owned(),
            source: e,
        })?;
        if path_in_tar.to_string_lossy() == want_inner {
            // Read and write out as the binary.
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| DistError::Archive {
                path: binary_path.to_owned(),
                source: e,
            })?;
            std::fs::write(binary_path, &buf).map_err(|e| DistError::Io {
                path: binary_path.to_owned(),
                source: e,
            })?;
            chmod_executable(binary_path)?;
            return Ok(());
        }
    }

    Err(DistError::BinaryNotInArchive {
        name: want_inner.to_owned(),
    })
}
