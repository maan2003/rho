//! Namespace setup must precede every thread, including the Rust test harness.
use std::sync::Arc;
use std::time::Duration;

use rho_notebook::Notebook;
#[path = "../../rho-fs-view/tests/common/workset.rs"]
mod common;
use rho_tool_shell::ShellTools;

fn main() {
    // These namespace-first binaries cannot use libtest, but nextest still
    // needs a libtest-compatible listing to run and time each binary.
    if std::env::args().any(|arg| arg == "--list") {
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("e2e: test");
        }
        return;
    }
    let unshare = std::process::Command::new("unshare")
        .args(["-U", "true"])
        .status();
    if !unshare.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping python_workspace: kernel forbids unshare(CLONE_NEWUSER)");
        return;
    }
    common::run("", |base| async move {
        let host_cwd = std::env::current_dir().unwrap();
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        let work = std::path::Path::new("/src");
        std::fs::create_dir_all(work.join("project")).unwrap();
        std::fs::write(work.join("project/value"), "host").unwrap();
        let view = base.for_cwd(camino::Utf8Path::new("/src/project")).unwrap();
        let wake = Arc::new(tokio::sync::Notify::new());
        let notebook = Notebook::new(
            ShellTools::new(Duration::from_secs(5), view),
            vec![],
            wake.clone(),
        )
        .unwrap();
        let cell = notebook.run("assert Path('value').read_text() == 'host'\nPath('value').write_text('python')\nprint(Path.cwd())\nimport os, subprocess\nos.chdir('/')\nassert Path.cwd() == Path('/')\nassert not Path('/home').joinpath(os.environ.get('USER', 'agent')).exists() or True".into());
        tokio::time::timeout(Duration::from_secs(10), async {
            while cell.facts().finished.is_none() {
                wake.notified().await;
            }
        })
        .await
        .unwrap();
        let end = cell.facts().finished.unwrap();
        let report = notebook.report().unwrap_or_default();
        assert!(!end.failed, "{report:?}");
        assert!(report.text.contains("/src/project"), "{report:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("project/value")).unwrap(),
            "python"
        );
        assert_eq!(std::env::current_dir().unwrap(), host_cwd);

        assert!(
            std::process::Command::new("kill")
                .args(["-INT", &std::process::id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::timeout(Duration::from_secs(2), interrupt.recv())
            .await
            .expect("Python import replaced the host SIGINT handler")
            .unwrap();
    });
}
