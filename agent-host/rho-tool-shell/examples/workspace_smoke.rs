//! End-to-end smoke test for the agent view: a workset, a clone through the
//! store, and shell tool commands inside the view namespace.
//! Run with `cargo run -p rho-tool-shell --example workspace_smoke`.

use rho_fs_view::{PathOverrides, StoreRefresh, StoreService, UserEnvironment, Worksets};
use rho_tool_shell::{ProcessEvent, ShellTools};

/// Runs `cmd` to the end and returns its output, failing unless it succeeds.
async fn shell(tools: &ShellTools, cmd: &str) -> anyhow::Result<String> {
    let mut process = tools.spawn(cmd, None, false).await?;
    let mut output = Vec::new();
    loop {
        match process.next().await {
            ProcessEvent::Output(bytes) => output.extend(bytes),
            ProcessEvent::Exited(status) => anyhow::ensure!(status.success(), "{cmd}: {status}"),
            ProcessEvent::Closed => break,
            ProcessEvent::Failed(error) => anyhow::bail!("{cmd}: {error}"),
        }
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
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
        unsafe {
            layout.build()?;
            layout.enter()?;
        }
        std::env::set_current_dir("/src/project")?;
        return tokio::runtime::Runtime::new()?.block_on(run_tools());
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

    let layout = rho_fs_view::WorksetLayout::new(&workset)?;
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

async fn run_tools() -> anyhow::Result<()> {
    let tools = ShellTools::in_directory("/src/project".into(), PathOverrides::default());

    let started = std::time::Instant::now();
    let output = shell(&tools, "pwd; cat file.txt").await?;
    println!("first call ({:?}):\n{output}", started.elapsed());
    assert!(
        output.contains("/src/project"),
        "commands start in the working directory"
    );
    assert!(output.contains("origin"), "the clone's files are visible");

    let output = shell(
        &tools,
        "echo agent > file.txt && git status --short && git log --oneline -1",
    )
    .await?;
    println!("write + git inside the view:\n{output}");
    let output = shell(
        &tools,
        "touch /tmp/scratch && ls / && test ! -e $HOME/.bashrc && echo home-is-empty",
    )
    .await?;
    println!("outside the workset:\n{output}");
    assert!(output.contains("home-is-empty"));

    println!("smoke test passed");
    Ok(())
}
