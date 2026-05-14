//! Unit tests for the distribution module.

use super::*;

#[test]
fn final_binary_relpath_unix() {
    struct Fake;
    #[async_trait::async_trait]
    impl BackendDistribution for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn resolve_version(&self, _: &VersionRequest) -> Result<String, DistError> {
            unreachable!()
        }
        async fn download_info(&self, _: &str, _: Platform) -> Result<DownloadInfo, DistError> {
            unreachable!()
        }
    }
    let p = Platform {
        os: Os::Macos,
        arch: Arch::Aarch64,
        libc: Libc::None,
    };
    assert_eq!(
        Fake.final_binary_relpath(p),
        std::path::PathBuf::from("bin/fake")
    );
}

#[test]
fn final_binary_relpath_windows() {
    struct Fake;
    #[async_trait::async_trait]
    impl BackendDistribution for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn resolve_version(&self, _: &VersionRequest) -> Result<String, DistError> {
            unreachable!()
        }
        async fn download_info(&self, _: &str, _: Platform) -> Result<DownloadInfo, DistError> {
            unreachable!()
        }
    }
    let p = Platform {
        os: Os::Windows,
        arch: Arch::X86_64,
        libc: Libc::None,
    };
    assert_eq!(
        Fake.final_binary_relpath(p),
        std::path::PathBuf::from("bin/fake.exe")
    );
}
