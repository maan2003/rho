//! Async test filesystem boundary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub struct TestDir(PathBuf);

impl TestDir {
    pub async fn new() -> Self {
        #[cfg(unix)]
        let root = PathBuf::from("/tmp");
        #[cfg(windows)]
        let root = std::env::temp_dir();
        let random = iroh::SecretKey::generate().public().to_string();
        let path = root.join(format!("ipc-{}", &random[..12]));
        tokio::fs::create_dir(&path).await.expect("test directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub async fn close(self) {
        tokio::fs::remove_dir_all(self.0)
            .await
            .expect("remove test directory");
    }
}
