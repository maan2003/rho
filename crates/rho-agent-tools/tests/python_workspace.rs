//! Namespace setup must precede every thread, including the Rust test harness.
use std::sync::Arc;
use std::time::Duration;

use rho_agent_tools::{PythonTool, SourceWaker, Tool, ToolHaste};
use rho_core::{ToolCall, ToolType};
use rho_tool_shell::ShellTools;
use rho_workspaces::{Repo, View};

fn main() {
    unsafe { rho_workspaces::init_daemon_namespace() }.unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let daemon_cwd = std::env::current_dir().unwrap();
        let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        // Managed workspaces need bcachefs, just like the workspace unit tests.
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let origin = temp.path().join("repo");
        std::fs::create_dir(&origin).unwrap();
        assert!(std::process::Command::new("jj")
            .current_dir(&origin)
            .args(["git", "init", "--no-colocate"])
            .status().unwrap().success());
        std::fs::write(origin.join("value"), "origin").unwrap();
        let repo = Arc::new(Repo::open(&origin).unwrap());
        let workspace = repo.create_workspace("@").await.unwrap();
        std::fs::write(workspace.checkout().join("value"), "workspace").unwrap();
        let view = View::new(vec![workspace.clone()]).unwrap();
        let tool = PythonTool::new(ShellTools::new(Duration::from_secs(5), view), vec![]).unwrap();
        let wake = Arc::new(tokio::sync::Notify::new());
        let mut cell = tool.run(ToolCall {
            id: "view".try_into().unwrap(),
            name: "exec".try_into().unwrap(),
            tool_type: ToolType::Custom,
            arguments: "assert Path('value').read_text() == 'workspace'\nPath('value').write_text('python')\nprint(Path.cwd())\nimport os, subprocess\nos.chdir('/')\nassert Path.cwd() == Path('/')".into(),
        }, SourceWaker::new(wake.clone()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(cell.haste(), ToolHaste::Ended { .. }) {
                wake.notified().await;
            }
        }).await.unwrap();
        let output = cell.first_output();
        assert_eq!(output.status, rho_core::ToolOutputStatus::Success, "{output:?}");
        assert!(output.output.contains(origin.to_str().unwrap()), "{output:?}");
        assert_eq!(std::fs::read_to_string(workspace.checkout().join("value")).unwrap(), "python");
        assert_eq!(std::fs::read_to_string(origin.join("value")).unwrap(), "origin");
        assert_eq!(std::env::current_dir().unwrap(), daemon_cwd);
        assert!(std::process::Command::new("kill")
            .args(["-INT", &std::process::id().to_string()])
            .status().unwrap().success());
        tokio::time::timeout(Duration::from_secs(2), interrupt.recv()).await
            .expect("Python import replaced the host SIGINT handler").unwrap();
    });
}
