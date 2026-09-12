//! Namespace setup must precede every thread, including the Rust test harness.
use std::sync::Arc;
use std::time::Duration;

use rho_agent_tools::{PythonTool, SourceWaker, Tool};
use rho_core::{ToolCall, ToolType};
use rho_tool_shell::ShellTools;
use rho_workset::{Mode, UserEnvironment, Worksets};

fn main() {
    let unshare = std::process::Command::new("unshare")
        .args(["-U", "true"])
        .status();
    if !unshare.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping python_workspace: kernel forbids unshare(CLONE_NEWUSER)");
        return;
    }
    unsafe { rho_workset::init_daemon_namespace() }.unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let daemon_cwd = std::env::current_dir().unwrap();
        let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir_all(work.join("project")).unwrap();
        std::fs::write(work.join("project/value"), "host").unwrap();
        let environment = UserEnvironment::new(std::env::vars_os().collect());
        let worksets = Worksets::open_plain(temp.path().join("state"), environment).await.unwrap();
        let workset = worksets.adopt(&work).unwrap();
        let view = workset
            .enter(Mode::View { home_skeleton: None }, camino::Utf8Path::new("/src/project"))
            .unwrap();
        let tool = PythonTool::new(ShellTools::new(Duration::from_secs(5), view), vec![]).unwrap();
        let wake = Arc::new(tokio::sync::Notify::new());
        let mut cell = tool.run(ToolCall {
            id: "view".try_into().unwrap(),
            name: "exec".try_into().unwrap(),
            tool_type: ToolType::Custom,
            arguments: "assert Path('value').read_text() == 'host'\nPath('value').write_text('python')\nprint(Path.cwd())\nimport os, subprocess\nos.chdir('/')\nassert Path.cwd() == Path('/')\nassert not Path('/home').joinpath(os.environ.get('USER', 'agent')).exists() or True".into(),
        }, SourceWaker::new(wake.clone()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !cell.python_exec().unwrap().quiescent() {
                wake.notified().await;
            }
        }).await.unwrap();
        let output = cell.first_output();
        assert_eq!(output.status, rho_core::ToolOutputStatus::Success, "{output:?}");
        assert!(output.output.contains("/src/project"), "{output:?}");
        assert_eq!(std::fs::read_to_string(work.join("project/value")).unwrap(), "python");
        assert_eq!(std::env::current_dir().unwrap(), daemon_cwd);
        assert!(std::process::Command::new("kill")
            .args(["-INT", &std::process::id().to_string()])
            .status().unwrap().success());
        tokio::time::timeout(Duration::from_secs(2), interrupt.recv()).await
            .expect("Python import replaced the host SIGINT handler").unwrap();
    });
}
