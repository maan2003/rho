//! Resolvers against the daemon's store over its socket, with the real
//! builder.

use std::path::PathBuf;
use std::sync::Arc;

use rho_devshell::{Client, Flake, Resolver};

#[tokio::test]
#[ignore = "manual: needs RHO_DEVSHELL_BUILDER and a flake checkout in RHO_TEST_FLAKE"]
async fn evaluates_once_then_hits_and_repins() {
    let flake = Flake::new(PathBuf::from(std::env::var("RHO_TEST_FLAKE").unwrap()).canonicalize().unwrap(), "default");
    let builder = PathBuf::from(std::env::var("RHO_DEVSHELL_BUILDER").unwrap());
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("cache");
    let store = Arc::new(
        rho_devshell_daemon::Store::open(rho_db::RhoDb::open(temp.path().join("db")), dir.clone()).await,
    );
    tokio::spawn(store.serve().unwrap());
    let resolver = || {
        Resolver::new(
            Some(Client::new(&dir)),
            dir.clone(),
            builder.clone(),
            std::env::vars_os().collect(),
        )
    };

    let start = std::time::Instant::now();
    let (first, _) = resolver().resolve(&flake).await.unwrap();
    eprintln!("miss: {:?}", start.elapsed());
    let root = rho_devshell::gc_root(&rho_devshell::roots_dir(&dir), &first.env_store_path);
    assert_eq!(std::fs::read_link(&root).unwrap(), PathBuf::from(&first.env_store_path));

    let start = std::time::Instant::now();
    let (hit, _) = resolver().resolve(&flake).await.unwrap();
    eprintln!("hit: {:?}", start.elapsed());
    assert_eq!(hit.id, first.id);

    // Losing the root is repaired by the next use.
    std::fs::remove_file(&root).unwrap();
    let (repinned, _) = resolver().resolve(&flake).await.unwrap();
    assert_eq!(repinned.id, first.id);
    assert_eq!(std::fs::read_link(&root).unwrap(), PathBuf::from(&first.env_store_path));

    // A watched resolver keeps the shell until an input changes.
    let watched = resolver().with_watcher(rho_watch::Watcher::global().unwrap());
    watched.resolve(&flake).await.unwrap();
    let start = std::time::Instant::now();
    let (kept, diagnostics) = watched.resolve(&flake).await.unwrap();
    eprintln!("kept: {:?}", start.elapsed());
    assert_eq!(kept.id, first.id);
    assert!(diagnostics.is_empty());

    // What `nix develop` in a view runs.
    let start = std::time::Instant::now();
    let shell = tokio::process::Command::new(&builder)
        .args(["shell".as_ref(), flake.dir.as_os_str()])
        .env("RHO_DEVSHELL_DIR", &dir)
        .output()
        .await
        .unwrap();
    eprintln!("builder shell: {:?}", start.elapsed());
    assert!(shell.status.success(), "{}", String::from_utf8_lossy(&shell.stderr));
    let shell: serde_json::Value = serde_json::from_slice(&shell.stdout).unwrap();
    assert_eq!(shell["env_store_path"], first.env_store_path.as_str());

    let activation = watched.activation(&hit.env_store_path).await.unwrap();
    let output = std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-c", rho_devshell::EXEC_SCRIPT, "bash"])
        .arg(&activation)
        .args(["bash", "-c", "command -v cargo"])
        .current_dir(&flake.dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    eprintln!("cargo: {}", String::from_utf8_lossy(&output.stdout));
}
