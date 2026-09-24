//! A workset's resolver against the daemon's store, with the real builder.

use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use rho_devshell::{Candidate, Flake, Resolver};

/// The store, in process instead of over the workset connection.
struct Direct(Arc<rho_devshell_daemon::Store>);

impl rho_devshell::Cache for Direct {
    fn lookup(&self, key: String) -> BoxFuture<'static, anyhow::Result<Vec<Candidate>>> {
        let store = self.0.clone();
        Box::pin(async move {
            Ok(store
                .lookup(&key)
                .await?
                .into_iter()
                .map(|c| Candidate {
                    id: c.id,
                    env_store_path: c.env_store_path,
                    data: c.data,
                })
                .collect())
        })
    }
    fn used(&self, id: u64) -> BoxFuture<'static, anyhow::Result<bool>> {
        let store = self.0.clone();
        Box::pin(async move { store.used(id).await })
    }
    fn store(&self, key: String, env: String, data: Vec<u8>) -> BoxFuture<'static, anyhow::Result<u64>> {
        let store = self.0.clone();
        Box::pin(async move { store.store(key, env, data).await })
    }
    fn forget(&self, id: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let store = self.0.clone();
        Box::pin(async move { store.forget(id).await })
    }
}

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
    let resolver = || {
        Resolver::new(
            Some(Arc::new(Direct(store.clone()))),
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
    assert!(first.activation.is_file());

    let start = std::time::Instant::now();
    let (hit, _) = resolver().resolve(&flake).await.unwrap();
    eprintln!("hit: {:?}", start.elapsed());
    assert_eq!(hit.id, first.id);

    // Losing the root is repaired by the next use.
    std::fs::remove_file(&root).unwrap();
    let (repinned, _) = resolver().resolve(&flake).await.unwrap();
    assert_eq!(repinned.id, first.id);
    assert_eq!(std::fs::read_link(&root).unwrap(), PathBuf::from(&first.env_store_path));

    let output = std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-c", rho_devshell::EXEC_SCRIPT, "bash"])
        .arg(&hit.activation)
        .args(["bash", "-c", "command -v cargo"])
        .current_dir(&flake.dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    eprintln!("cargo: {}", String::from_utf8_lossy(&output.stdout));
}
