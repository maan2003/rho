//! The rendezvous directory and everything in it are the user's alone,
//! by explicit mode, never via umask.

mod common;

use std::os::unix::fs::PermissionsExt;

use iroh::SecretKey;
use iroh_ipc_transport::{IpcTransport, LocalDiscovery};

async fn mode_of(path: &std::path::Path) -> u32 {
    tokio::fs::metadata(path).await.unwrap().permissions().mode() & 0o7777
}

async fn mode_after_bind(discovery: LocalDiscovery) -> u32 {
    let id = SecretKey::generate().public();
    let owner = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    owner.close().await.unwrap();
    mode_of(discovery.root()).await
}

#[tokio::test]
async fn the_root_is_created_owner_only() {
    let dir = common::TestDir::new().await;
    let discovery = LocalDiscovery::new(dir.path().join("private"));
    assert_eq!(mode_after_bind(discovery).await, 0o700);
    dir.close().await;
}

#[tokio::test]
async fn existing_root_mode_is_never_changed() {
    let dir = common::TestDir::new().await;
    let root = dir.path().join("existing");
    tokio::fs::create_dir(&root).await.unwrap();
    tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750))
        .await
        .unwrap();
    assert_eq!(mode_after_bind(LocalDiscovery::new(&root)).await, 0o750);
    dir.close().await;
}

#[tokio::test]
async fn every_file_a_bind_makes_is_private() {
    let dir = common::TestDir::new().await;
    let discovery = LocalDiscovery::new(dir.path().join("files"));
    let id = SecretKey::generate().public();
    let owner = IpcTransport::prepare(discovery.clone(), id).await.unwrap();
    let mut entries = tokio::fs::read_dir(discovery.root()).await.unwrap();
    let mut seen = Vec::new();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert_eq!(mode_of(&entry.path()).await, 0o600, "{name}");
        seen.push(name);
    }
    seen.sort();
    assert_eq!(seen.len(), 4, "socket, record, lease and the lock: {seen:?}");
    owner.close().await.unwrap();
    dir.close().await;
}
