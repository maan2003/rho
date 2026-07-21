//! End-to-end smoke test for workspace isolation: daemon namespace, jj
//! workspace creation, ws-parent pointer resolution, and namespaced command
//! execution.
//! Run with `cargo run -p rho-tool-shell --example workspace_smoke`.

use std::sync::Arc;
use std::time::Duration;

use rho_core::{ToolCall, ToolCallId, ToolName, ToolType};
use rho_tool_shell::{EXEC_COMMAND_TOOL_NAME, ShellTools};
use rho_workspaces::{Repo, View};

fn shell_call(command: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from(EXEC_COMMAND_TOOL_NAME).unwrap(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({ "command": command }).to_string(),
    }
}

fn slow_shell_call(command: &str) -> ToolCall {
    let mut call = shell_call(command);
    call.arguments = serde_json::json!({ "command": command, "yield_time_ms": 30_000 }).to_string();
    call
}

fn jj(repo: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("jj")
        .arg("--repository")
        .arg(repo)
        .args(args)
        .output()
        .expect("run jj");
    assert!(
        output.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn main() -> anyhow::Result<()> {
    // SAFETY: top of main, single-threaded.
    unsafe { rho_workspaces::init_daemon_namespace() }?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // Managed workspaces require bcachefs, so keep the smoke repository on
    // the same filesystem as this checkout rather than /tmp.
    let temp = tempfile::tempdir_in(std::env::current_dir()?)?;
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo)?;
    let status = std::process::Command::new("jj")
        .current_dir(&repo)
        .args(["git", "init", "--colocate"])
        .status()?;
    assert!(status.success());
    std::fs::write(repo.join("file.txt"), "origin\n")?;
    jj(&repo, &["commit", "-m", "init"]);
    std::fs::write(repo.join("file.txt"), "origin\n")?;
    jj(&repo, &["st"]);

    let temp = if std::env::var_os("SMOKE_KEEP").is_some() {
        let path = temp.keep();
        println!("keeping temp dir: {}", path.display());
        None
    } else {
        Some(temp)
    };
    let _ = &temp;
    let repo_handle = Arc::new(Repo::open(&repo)?);
    let started = std::time::Instant::now();
    let workspace = repo_handle.create_workspace("@").await?;
    let main_id = workspace.info().workspace_id().unwrap();
    println!("created workspace in {:?}", started.elapsed());
    println!("repo: {}", workspace.repo());
    println!("checkout: {}", workspace.checkout());
    assert!(
        workspace
            .checkout()
            .starts_with(repo.join(".jj/managed-workspaces")),
        "managed workspaces live inside the repo"
    );

    let tools = ShellTools::new(
        Duration::from_secs(30),
        View::new(vec![Arc::clone(&workspace)])?,
    );

    let started = std::time::Instant::now();
    let result = tools.call(shell_call("pwd; cat file.txt")).await;
    println!("first call ({:?}):\n{}", started.elapsed(), result.output);
    assert!(
        result.output.contains(repo.to_str().unwrap()),
        "agent should see the origin repo path"
    );

    let result = tools
        .call(shell_call("echo agent > file.txt && jj st"))
        .await;
    println!("write + jj inside namespace:\n{}", result.output);

    // Git must work in the namespace too: the checkout's `.git` gitdir pointer
    // was rewritten through ws-parent.
    let result = tools
        .call(slow_shell_call(
            "status=\"$(git status --short)\" && printf '%s\\n' \"$status\" && \
             test \"$status\" = \" M file.txt\" && \
             git log --oneline | head -2 && git rev-parse --show-toplevel",
        ))
        .await;
    println!("git inside namespace:\n{}", result.output);
    assert!(
        result.output.contains("Process exited with code 0"),
        "git should work inside the namespace"
    );
    assert!(
        result.output.contains("file.txt"),
        "git should see the agent's dirty file"
    );
    assert!(
        result.output.contains(repo.to_str().unwrap()),
        "git should report the origin path as its toplevel"
    );

    // Real git (unwrapped, via nix): diff and commit must work against the
    // workspace's git worktree.
    let result = tools
        .call(slow_shell_call(
            "nix shell nixpkgs#git -c sh -c \
             'git diff --stat && git commit -am from-git && \
              git ls-files --error-unmatch file.txt && \
              test \"$(git show HEAD:file.txt)\" = agent && \
              git log --oneline | head -1'",
        ))
        .await;
    println!(
        "real git diff + commit inside namespace:\n{}",
        result.output
    );
    assert!(
        result.output.contains("Process exited with code 0"),
        "git diff/commit should work inside the namespace"
    );
    assert!(
        result.output.contains("file.txt") && result.output.contains("from-git"),
        "git should diff and commit the agent's edit"
    );

    // The origin checkout must be untouched; the checkout has the agent's edit.
    assert_eq!(std::fs::read_to_string(repo.join("file.txt"))?, "origin\n");
    assert_eq!(
        std::fs::read_to_string(workspace.checkout().join("file.txt"))?,
        "agent\n"
    );

    // From the host (no agent namespace), the checkout is an ordinary jj
    // workspace: cd in and run jj — the pointer resolves via the origin's
    // ws-parent symlink.
    let output = std::process::Command::new("jj")
        .current_dir(workspace.checkout())
        .arg("st")
        .output()?;
    assert!(
        output.status.success(),
        "host-side jj inside the checkout should work: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("host-side jj in checkout: ok");

    let started = std::time::Instant::now();
    workspace.snapshot().await?;
    println!("snapshot in {:?}", started.elapsed());

    let output = std::process::Command::new("jj")
        .arg("--repository")
        .arg(&repo)
        .args(["log", "--no-graph", "-T", "separate(\" \", change_id.short(), working_copies, description.first_line()) ++ \"\\n\""])
        .output()?;
    println!(
        "origin log:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let started = std::time::Instant::now();
    let result = tools.call(shell_call("true")).await;
    assert!(result.output.contains("Process exited with code 0"));
    println!("steady-state call round trip: {:?}", started.elapsed());

    // Joining a workspace shares the live instance: same checkout, same
    // mount namespace, edits visible to each other instantly (no snapshot).
    let joined = repo_handle.open_workspace(main_id).await?;
    assert!(
        Arc::ptr_eq(&workspace, &joined),
        "join shares the live workspace instance"
    );
    let tools_joined = ShellTools::new(Duration::from_secs(30), View::new(vec![joined])?);
    let result = tools_joined
        .call(shell_call("echo joint > joint.txt"))
        .await;
    assert!(result.output.contains("Process exited with code 0"));
    let result = tools.call(shell_call("cat joint.txt")).await;
    assert!(
        result.output.contains("joint"),
        "joining agent's edit visible to the original instantly: {}",
        result.output
    );
    println!("join shares checkout across separate views: ok");

    // A user-checkout workspace works directly in the user's own checkout:
    // real repo path, no namespace, edits land in the origin immediately.
    let uc = repo_handle.user_checkout().await?;
    let tools_uc = ShellTools::new(Duration::from_secs(30), View::new(vec![uc])?);
    let result = tools_uc.call(shell_call("pwd && echo here > uc.txt")).await;
    assert!(result.output.contains(repo.to_str().unwrap()));
    assert_eq!(std::fs::read_to_string(repo.join("uc.txt"))?, "here\n");
    println!("user-checkout workspace edits the origin directly: ok");

    // ---- multi-workspace snapshot matrix ----
    // Two independent sibling workspaces, both children of the user's @.
    let wa = repo_handle.create_workspace("@").await?;
    let wb = repo_handle.create_workspace("@").await?;
    let agent_a_id = wa.info().workspace_id().unwrap();
    let agent_b_id = wb.info().workspace_id().unwrap();
    let tools_a = ShellTools::new(Duration::from_secs(30), View::new(vec![Arc::clone(&wa)])?);

    // (1) Snapshot from OUTSIDE (origin jj): the user's edit follows down
    // into both checkouts via rebase_descendants, sibling edits stay isolated
    // from each other, and nothing leaks up into the user's checkout.
    std::fs::write(wa.checkout().join("a.txt"), "one\n")?;
    std::fs::write(wb.checkout().join("b.txt"), "two\n")?;
    std::fs::write(repo.join("u.txt"), "user\n")?;
    jj(&repo, &["st"]);
    assert!(
        wa.checkout().join("u.txt").exists(),
        "user edit follows into checkout a"
    );
    assert!(
        wb.checkout().join("u.txt").exists(),
        "user edit follows into checkout b"
    );
    assert!(
        !wa.checkout().join("b.txt").exists(),
        "sibling edits stay isolated"
    );
    assert!(
        !wb.checkout().join("a.txt").exists(),
        "sibling edits stay isolated"
    );
    assert!(
        !repo.join("a.txt").exists(),
        "agent work must not leak into the user's checkout"
    );
    println!("outside snapshot: parent following + sibling isolation: ok");

    // (2) Snapshot from INSIDE an agent namespace: the agent's own jj must
    // load the user's workspace and the sibling checkout through
    // namespace-local paths.
    std::fs::write(repo.join("u2.txt"), "user\n")?;
    let result = tools_a.call(shell_call("jj st")).await;
    assert!(
        result.output.contains("Process exited with code 0"),
        "jj st in namespace: {}",
        result.output
    );
    assert!(
        wa.checkout().join("u2.txt").exists(),
        "user edit followed from inside the namespace"
    );
    println!("inside-namespace snapshot: ok");

    // (3) Snapshot from inside a managed checkout on the host: the sibling's
    // dirty file gets committed into its change.
    std::fs::write(wa.checkout().join("d.txt"), "dee\n")?;
    let output = std::process::Command::new("jj")
        .current_dir(wb.checkout())
        .args(["log", "--no-graph", "-r"])
        .arg(format!("ws-{}@", agent_a_id.encoded()))
        .args(["-T", "diff.files().len()"])
        .output()?;
    assert!(
        output.status.success(),
        "host-side jj in sibling checkout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "sibling checkout snapshotted when running jj from another checkout"
    );
    println!("host-side in-checkout snapshot: ok");

    // ---- daemon restart: stable reopen ----
    // A fresh Repo handle reuses the same managed path. Other live managed
    // workspaces are untouched; there is no repo-wide detach/reap operation.
    let wb_path = wb.checkout().to_owned();
    drop(wb);
    let repo_handle2 = Arc::new(Repo::open(&repo)?);
    let wb2 = repo_handle2.open_workspace(agent_b_id).await?;
    assert_eq!(wb2.checkout(), wb_path);
    let tools_b2 = ShellTools::new(Duration::from_secs(30), View::new(vec![Arc::clone(&wb2)])?);
    let result = tools_b2.call(shell_call("cat b.txt")).await;
    assert!(
        result.output.contains("two"),
        "reattached workspace has its work back: {}",
        result.output
    );
    println!("stable managed reopen: ok");

    // ---- multi-workdir view: two repos in one namespace ----
    // A view over two workdirs mounts both entries' checkouts over their origin
    // paths: commands see the agent's workspace checkout in each repo, at
    // the repo's real path.
    let repo2 = repo.parent().unwrap().join("repo2");
    std::fs::create_dir(&repo2)?;
    let status = std::process::Command::new("jj")
        .current_dir(&repo2)
        .args(["git", "init", "--colocate"])
        .status()?;
    assert!(status.success());
    std::fs::write(repo2.join("file.txt"), "user\n")?;
    jj(&repo2, &["commit", "-m", "init"]);
    std::fs::write(repo2.join("file.txt"), "user\n")?;
    jj(&repo2, &["st"]);
    let repo2_handle = Arc::new(Repo::open(&repo2)?);
    let repo2_ws = repo2_handle.create_workspace("@").await?;
    // Distinguish the workspace checkout from the user's live files.
    std::fs::write(repo2_ws.checkout().join("file.txt"), "workspace\n")?;

    let view = View::new(vec![Arc::clone(&wb2), Arc::clone(&repo2_ws)])?;
    let tools_multi = ShellTools::new(Duration::from_secs(30), Arc::clone(&view));
    let repo2_path = repo2.display();
    let result = tools_multi
        .call(shell_call(&format!("cat {repo2_path}/file.txt")))
        .await;
    assert!(
        result.output.contains("workspace"),
        "the second workdir shows the workspace checkout at its real path: {}",
        result.output
    );
    assert!(
        View::new(vec![Arc::clone(&wb2), Arc::clone(&wb2)]).is_err(),
        "a view with the same repo twice must be rejected"
    );
    println!("multi-workdir view: ok");

    println!("smoke test passed");
    Ok(())
}
