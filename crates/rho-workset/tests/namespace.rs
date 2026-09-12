//! Live-namespace behaviour: bounded reads through the checkout map and the
//! Claude home mount stack. Runs without the libtest harness because the
//! identity user namespace must be created while the process is still
//! single-threaded.

use std::path::{Path, PathBuf};
use std::process::Command;

use rho_workset::{ClaudeHome, MAX_BOUNDED_READ, Mode};

mod common;
use common::{jj_binary, open_worksets, setup_remote};

fn main() {
    let unshare = Command::new("unshare").args(["-U", "true"]).status();
    if !unshare.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping namespace test: kernel forbids unshare(CLONE_NEWUSER)");
        return;
    }
    // Build jj before entering the user namespace so cargo runs as the host user.
    let jj_bin = jj_binary();
    // SAFETY: no threads exist yet.
    unsafe { rho_workset::init_daemon_namespace() }.unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(run(jj_bin));
}

fn host_binary(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| std::fs::canonicalize(candidate).ok())
        .unwrap_or_else(|| panic!("{name} not on PATH"))
}

async fn run(jj_bin: PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let (_source, remote) = setup_remote(temp.path());
    let root = open_worksets(temp.path(), &jj_bin);
    let workset = root.create().await.unwrap();
    let checkout = workset
        .clone(
            "repo",
            remote.to_str().unwrap(),
            Some("project"),
            Some("main@origin"),
        )
        .await
        .unwrap();
    let outside = temp.path().join("outside.txt");
    std::fs::write(&outside, "secret\n").unwrap();
    std::os::unix::fs::symlink(&outside, checkout.checkout().join("escape")).unwrap();
    std::fs::create_dir(checkout.checkout().join("sub")).unwrap();
    std::os::unix::fs::symlink("../file.txt", checkout.checkout().join("sub/inside")).unwrap();

    let ns = workset
        .enter(Mode::View {
            home_skeleton: None,
        })
        .await
        .unwrap();

    // Bounded reads: visible and relative paths, limits, and escapes.
    assert_eq!(
        ns.read_file_bounded(Path::new("/src/project/file.txt"), 1024)
            .await
            .unwrap(),
        b"one\n"
    );
    assert_eq!(
        ns.read_file_bounded(Path::new("file.txt"), 4)
            .await
            .unwrap(),
        b"one\n"
    );
    assert_eq!(
        ns.read_file_bounded(Path::new("sub/inside"), 1024)
            .await
            .unwrap(),
        b"one\n",
        "symlinks staying inside the checkout resolve"
    );
    assert!(
        ns.read_file_bounded(Path::new("file.txt"), 3)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("file.txt"), MAX_BOUNDED_READ + 1)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("escape"), 1024)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("../outside.txt"), 1024)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("/src/project/../../etc/passwd"), 1024)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("/src/other/file.txt"), 1024)
            .await
            .is_err()
    );
    assert!(ns.read_file_bounded(Path::new("sub"), 1024).await.is_err());

    // Claude home: mounted into the live namespace, replaceable.
    let host_home = dirs::home_dir().unwrap();
    let account = |name: &str, prompt: &str, settings: &str| {
        let account = temp.path().join(name);
        std::fs::create_dir_all(account.join("projects")).unwrap();
        std::fs::write(account.join("CLAUDE.md"), "").unwrap();
        std::fs::write(account.join("settings.json"), "").unwrap();
        std::fs::write(account.join("marker"), format!("{name}\n")).unwrap();
        std::fs::write(temp.path().join(format!("{name}-prompt.md")), prompt).unwrap();
        std::fs::write(temp.path().join(format!("{name}-settings.json")), settings).unwrap();
        account
    };
    let shared = temp.path().join("shared-projects");
    std::fs::create_dir_all(shared.join("shared-marker")).unwrap();
    let account_one = account("account-one", "PROMPT ONE\n", "{\"one\":true}\n");
    let account_two = account("account-two", "PROMPT TWO\n", "{\"two\":true}\n");
    let home_one = ClaudeHome {
        account: account_one,
        config_home: host_home.join(".claude"),
        shared_projects: shared.clone(),
        prompt: temp.path().join("account-one-prompt.md"),
        settings: Some(temp.path().join("account-one-settings.json")),
    };
    let home_two = ClaudeHome {
        account: account_two,
        config_home: host_home.join(".claude"),
        shared_projects: shared.clone(),
        prompt: temp.path().join("account-two-prompt.md"),
        settings: None,
    };

    let sh = host_binary("sh");
    let script = r#"
        read -r line < "$HOME/.claude/CLAUDE.md"; echo "prompt=$line"
        read -r line < "$HOME/.claude/settings.json" || true; echo "settings=$line"
        read -r line < "$HOME/.claude/marker"; echo "marker=$line"
        for entry in "$HOME"/.claude/projects/*; do echo "project=${entry##*/}"; done
        echo "home=$HOME"
    "#;
    let observe = |ns: &rho_workset::Namespace| {
        let mut command = tokio::process::Command::new(&sh);
        command.arg("-c").arg(script);
        ns.prepare_command(&mut command, None).unwrap();
        async move {
            let output = command.output().await.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        }
    };

    ns.set_claude_home(home_one.clone()).await.unwrap();
    let seen = observe(&ns).await;
    assert!(seen.contains("prompt=PROMPT ONE\n"), "{seen}");
    assert!(seen.contains("settings={\"one\":true}\n"), "{seen}");
    assert!(seen.contains("marker=account-one\n"), "{seen}");
    assert!(seen.contains("project=shared-marker\n"), "{seen}");
    assert!(seen.contains("home=/home/agent\n"), "{seen}");

    // Same home again is a no-op; a different one replaces the stack.
    ns.set_claude_home(home_one.clone()).await.unwrap();
    ns.set_claude_home(home_two.clone()).await.unwrap();
    let seen = observe(&ns).await;
    assert!(seen.contains("prompt=PROMPT TWO\n"), "{seen}");
    assert!(seen.contains("settings=\n"), "{seen}");
    assert!(seen.contains("marker=account-two\n"), "{seen}");
    assert!(seen.contains("project=shared-marker\n"), "{seen}");

    // Writes through the mount land in the host account directory.
    let mut command = tokio::process::Command::new(&sh);
    command
        .arg("-c")
        .arg("echo written > \"$HOME/.claude/projects/from-agent\"");
    ns.prepare_command(&mut command, None).unwrap();
    assert!(command.status().await.unwrap().success());
    assert_eq!(
        std::fs::read_to_string(shared.join("from-agent")).unwrap(),
        "written\n"
    );

    // A missing account fails cleanly and leaves the previous home mounted.
    let broken = ClaudeHome {
        account: temp.path().join("missing"),
        ..home_two.clone()
    };
    assert!(ns.set_claude_home(broken).await.is_err());
    let seen = observe(&ns).await;
    assert!(seen.contains("prompt=PROMPT TWO\n"), "{seen}");
    // The host home was never touched.
    assert!(
        !host_home.join(".claude/marker").exists() || {
            std::fs::read_to_string(host_home.join(".claude/marker"))
                .map(|marker| !marker.starts_with("account-"))
                .unwrap_or(true)
        }
    );
    println!("namespace test passed");
}
