//! Host platform detection.
//!
//! Each backend formats `Platform` differently for its own URL scheme
//! (claude uses `darwin-arm64`, codex uses Rust target triples like
//! `aarch64-apple-darwin`). The detection logic is shared.

use super::error::DistError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
    /// Only meaningful on Linux; `None` elsewhere.
    pub libc: Libc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Libc {
    /// Not linux, or linux but not relevant.
    None,
    /// Mainstream linux distros.
    Glibc,
    /// Alpine, distroless, etc.
    Musl,
}

/// Detect the platform anatta is currently running on.
pub fn detect_platform() -> Result<Platform, DistError> {
    let os = if cfg!(target_os = "macos") {
        Os::Macos
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        return Err(DistError::UnsupportedPlatform {
            detail: format!("unrecognised target_os: {}", std::env::consts::OS),
        });
    };

    let arch = if cfg!(target_arch = "x86_64") {
        Arch::X86_64
    } else if cfg!(target_arch = "aarch64") {
        Arch::Aarch64
    } else {
        return Err(DistError::UnsupportedPlatform {
            detail: format!("unrecognised target_arch: {}", std::env::consts::ARCH),
        });
    };

    let libc = match os {
        Os::Linux => detect_linux_libc(),
        _ => Libc::None,
    };

    Ok(Platform { os, arch, libc })
}

#[cfg(target_os = "linux")]
fn detect_linux_libc() -> Libc {
    // Heuristic: presence of musl loader files / `ldd` output.
    if std::path::Path::new("/lib/libc.musl-x86_64.so.1").exists()
        || std::path::Path::new("/lib/libc.musl-aarch64.so.1").exists()
        || std::path::Path::new("/lib/ld-musl-x86_64.so.1").exists()
        || std::path::Path::new("/lib/ld-musl-aarch64.so.1").exists()
    {
        return Libc::Musl;
    }
    Libc::Glibc
}

#[cfg(not(target_os = "linux"))]
fn detect_linux_libc() -> Libc {
    Libc::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_platform_resolves() {
        let p = detect_platform().unwrap();
        // On macOS dev box this should be one of these.
        if p.os == Os::Macos {
            assert!(matches!(p.arch, Arch::Aarch64 | Arch::X86_64));
            assert_eq!(p.libc, Libc::None);
        }
    }
}
