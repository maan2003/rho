//! End-to-end smoke test for the agent view: a workset, a clone through the
//! store, and shell tool commands inside the view namespace.
//! Run with `cargo run -p rho-tool-shell --example workspace_smoke`.

use std::sync::Arc;
use std::time::Duration;

use camino::Utf8Path;
use rho_agent_types::{ToolCall, ToolCallId, ToolName, ToolType};
use rho_fs_view::{Mode, StoreRefresh, StoreService, UserEnvironment, Worksets};
use rho_tool_shell::{EXEC_COMMAND_TOOL_NAME, ShellTools};

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
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "--inside") {
        let bytes = std::fs::read(&args[2])?;
        let layout: rho_fs_view::WorksetLayout = senax_encoder::decode(&mut bytes.as_slice())
            .map_err(|_| anyhow::anyhow!("invalid layout"))?;
        let view = unsafe {
            layout.build()?;
            layout.enter()?
        }
        .for_cwd(Utf8Path::new("/src/project"))?;
        return tokio::runtime::Runtime::new()?.block_on(run_tools(view));
    }
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

    let mount_root = temp.path().join("mount");
    std::fs::create_dir(&mount_root)?;
    let layout = rho_fs_view::WorksetLayout::new(
        &workset,
        Mode::View {
            home_skeleton: None,
        },
        camino::Utf8PathBuf::from_path_buf(mount_root)
            .map_err(|_| anyhow::anyhow!("non-UTF8 root"))?,
    )?;
    let path = temp.path().join("layout");
    std::fs::write(
        &path,
        senax_encoder::encode(&layout).map_err(|_| anyhow::anyhow!("encode layout"))?,
    )?;
    let status = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--inside")
        .arg(path)
        .status()
        .await?;
    anyhow::ensure!(status.success(), "workset smoke failed");
    assert_eq!(
        std::fs::read_to_string(checkout.join("file.txt"))?,
        "agent\n"
    );
    assert!(!temp.path().join("scratch").exists());
    Ok(())
}

async fn run_tools(view: Arc<rho_fs_view::Namespace>) -> anyhow::Result<()> {
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
            "echo agent > file.txt && git status --short && git log --oneline -1",
        ))
        .await;
    println!("write + git inside the view:\n{}", result.output);
    assert!(
        result.output.contains("Process exited with code 0"),
        "git should work inside the view"
    );
    let result = tools
        .call(shell_call(
            "touch /tmp/scratch && ls / && test ! -e $HOME/.bashrc && echo home-is-empty",
        ))
        .await;
    println!("outside the workset:\n{}", result.output);
    assert!(result.output.contains("home-is-empty"));

    println!("smoke test passed");
    Ok(())
}
