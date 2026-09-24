//! Resolvers against the agent host's store over its socket, with the real
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
    for _ in 0..100 {
        watched.resolve(&flake).await.unwrap();
    }
    eprintln!("kept: {:?}", start.elapsed() / 100);
    let (kept, diagnostics) = watched.resolve(&flake).await.unwrap();
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

    // A script the daemon removed is written again.
    let activation = watched.activation(&hit.env_store_path).await.unwrap();
    std::fs::remove_dir_all(rho_devshell::activations_dir(&dir, &hit.env_store_path)).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(watched.activation(&hit.env_store_path).await.unwrap(), activation);
    eprintln!("activate: {:?}", start.elapsed());
    assert!(activation.exists());
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

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
    assert!(status.success(), "git {args:?}");
}

/// What evaluation observed decides which cached shells hold: in a git
/// flake, reads of tracked files and the index's visibility.
#[tokio::test]
#[ignore = "manual: needs RHO_DEVSHELL_BUILDER and RHO_TEST_NIXPKGS, a nixpkgs source in the store"]
async fn observations_decide_hits() {
    let builder = PathBuf::from(std::env::var("RHO_DEVSHELL_BUILDER").unwrap());
    let nixpkgs = std::env::var("RHO_TEST_NIXPKGS").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("cache");
    let store = Arc::new(
        rho_devshell_daemon::Store::open(rho_db::RhoDb::open(temp.path().join("db")), dir.clone()).await,
    );
    tokio::spawn(store.serve().unwrap());
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(
        repo.join("flake.nix"),
        format!(
            r#"{{
  inputs.nixpkgs.url = "path:{nixpkgs}";
  outputs = {{ self, nixpkgs }}: let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {{
    devShells.x86_64-linux.default = pkgs.mkShellNoCC {{
      FOO = builtins.readFile ./data.txt;
      MAYBE = if builtins.pathExists ./maybe then "yes" else "no";
    }};
  }};
}}
"#
        ),
    )
    .unwrap();
    std::fs::write(repo.join("data.txt"), "one").unwrap();
    std::fs::write(repo.join("other.txt"), "x").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    let status = std::process::Command::new("nix")
        .args(["flake", "lock", "--extra-experimental-features", "nix-command flakes"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(status.success());
    git(&repo, &["add", "flake.lock"]);

    let resolver = || {
        Resolver::new(Some(Client::new(&dir)), dir.clone(), builder.clone(), std::env::vars_os().collect())
    };
    let id = |flake: &Flake| {
        let flake = flake.clone();
        let resolver = resolver();
        async move { resolver.resolve(&flake).await.unwrap().0.id.unwrap() }
    };
    let flake = Flake::new(repo.canonicalize().unwrap(), "default");
    let first = id(&flake).await;
    assert_eq!(id(&flake).await, first, "hit");

    std::fs::write(repo.join("other.txt"), "y").unwrap();
    assert_eq!(id(&flake).await, first, "a file evaluation did not read");
    std::fs::write(repo.join("maybe"), "").unwrap();
    assert_eq!(id(&flake).await, first, "an untracked file is invisible");
    git(&repo, &["add", "maybe"]);
    let tracked = id(&flake).await;
    assert_ne!(tracked, first, "a checked path appeared");
    std::fs::write(repo.join("data.txt"), "two").unwrap();
    let edited = id(&flake).await;
    assert!(edited != first && edited != tracked, "a read file changed");
    std::fs::write(repo.join("data.txt"), "one").unwrap();
    git(&repo, &["rm", "-q", "--cached", "maybe"]);
    assert_eq!(id(&flake).await, first, "back to the first shell's inputs");

    let copy = temp.path().join("copy");
    let status = std::process::Command::new("cp").arg("-a").arg(&repo).arg(&copy).status().unwrap();
    assert!(status.success());
    assert_eq!(id(&Flake::new(copy.canonicalize().unwrap(), "default")).await, first, "a copy shares entries");

    let watched = resolver().with_watcher(rho_watch::Watcher::global().unwrap());
    assert_eq!(watched.resolve(&flake).await.unwrap().0.id, Some(first));
    let start = std::time::Instant::now();
    assert_eq!(watched.resolve(&flake).await.unwrap().0.id, Some(first));
    assert!(start.elapsed() < std::time::Duration::from_millis(50), "kept: {:?}", start.elapsed());
    // `maybe` is untracked now: data "two" without it is a new shell.
    std::fs::write(repo.join("data.txt"), "two").unwrap();
    let changed = watched.resolve(&flake).await.unwrap().0.id.unwrap();
    assert!(![first, tracked, edited].contains(&changed), "a watched read changed");
}
