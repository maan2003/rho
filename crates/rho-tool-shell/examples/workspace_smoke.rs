//! End-to-end smoke test for the agent view: a workset, a clone through the
//! store, and shell tool commands inside the view namespace.
//! Run with `cargo run -p rho-tool-shell --example workspace_smoke`.

use std::sync::Arc;
use std::time::Duration;

use camino::Utf8Path;
use rho_core::{ToolCall, ToolCallId, ToolName, ToolType};
use rho_tool_shell::{EXEC_COMMAND_TOOL_NAME, ShellTools};
use rho_workset::{Mode, StoreRefresh, StoreService, UserEnvironment, Worksets};

fn shell_call(command: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from(EXEC_COMMAND_TOOL_NAME).unwrap(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({ "command": command }).to_string(),
    }
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Smoke", "-c", "user.email=smoke@localhost"])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn main() -> anyhow::Result<()> {
    // SAFETY: top of main, single-threaded.
    unsafe { rho_workset::init_daemon_namespace() }?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    std::fs::create_dir(&source)?;
    git(&source, &["init", "-q", "-b", "main"]);
    std::fs::write(source.join("file.txt"), "origin\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "-qm", "init"]);

    let environment = UserEnvironment::new(std::env::vars_os().collect());
    let worksets = Worksets::open(
        temp.path().join("state"),
        environment,
        Default::default(),
        StoreService::Serve(StoreRefresh::default()),
    )
    .await?;
    let workset = worksets.create().await?;
    let started = std::time::Instant::now();
    let checkout = workset
        .clone_repo(source.to_str().unwrap(), Some("project"))
        .await?;
    println!(
        "cloned through the store in {:?}: {checkout}",
        started.elapsed()
    );

    let view = workset.enter(
        Mode::View {
            home_skeleton: None,
        },
        Utf8Path::new("/src/project"),
    )?;
    let tools = ShellTools::new(Duration::from_secs(30), Arc::clone(&view));

    let started = std::time::Instant::now();
    let result = tools.call(shell_call("pwd; cat file.txt")).await;
    println!("first call ({:?}):\n{}", started.elapsed(), result.output);
    assert!(
        result.output.contains("/src/project"),
        "commands start in the working directory"
    );
    assert!(
        result.output.contains("origin"),
        "the clone's files are visible"
    );

    let result = tools
        .call(shell_call(
            "echo agent > file.txt && jj st && jj log --no-graph -r @ -T description",
        ))
        .await;
    println!("write + jj inside the view:\n{}", result.output);
    assert!(
        result.output.contains("Process exited with code 0"),
        "jj should work inside the view"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("file.txt"))?,
        "agent\n",
        "edits land in the workset on the host"
    );

    let result = tools
        .call(shell_call(
            "touch /tmp/scratch && ls / && test ! -e $HOME/.bashrc && echo home-is-empty",
        ))
        .await;
    println!("outside the workset:\n{}", result.output);
    assert!(result.output.contains("home-is-empty"));
    assert!(!temp.path().join("scratch").exists());

    println!("smoke test passed");
    Ok(())
}
