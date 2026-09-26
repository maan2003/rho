use std::sync::Arc;
use std::time::Duration;

use rho_fs_view::PathOverrides;
use rho_tool_shell::ShellTools;
use tokio::sync::Notify;

use crate::{CellHandle, Notebook};

fn notebook() -> (Notebook, Arc<Notify>) {
    let shell = ShellTools::in_directory(
        Duration::from_secs(5),
        "/tmp".into(),
        PathOverrides::default(),
    );
    let wake = Arc::new(Notify::new());
    (
        Notebook::new(shell, Vec::new(), Arc::clone(&wake)).unwrap(),
        wake,
    )
}

async fn until(wake: &Notify, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            wake.notified().await;
        }
    })
    .await
    .expect("timed out");
}

async fn finished(wake: &Notify, cell: &CellHandle) {
    until(wake, || cell.facts().finished.is_some()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cell_that_ends_first_speaks_plainly() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("print(6 * 7)".into());
    finished(&wake, &cell).await;
    assert_eq!(notebook.report().unwrap().text, "42");
    assert!(notebook.report().is_none());
    assert!(notebook.facts().is_empty());

    let silent = notebook.run("x = 1".into());
    finished(&wake, &silent).await;
    assert_eq!(notebook.report().unwrap().text, "Cell finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_raise_is_the_cells_output() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("raise ValueError('nope')".into());
    finished(&wake, &cell).await;
    assert!(cell.facts().finished.unwrap().failed);
    assert!(notebook.report().unwrap().text.contains("ValueError: nope"));
}

#[tokio::test(flavor = "multi_thread")]
async fn streamed_statements_run_as_they_arrive() {
    let (notebook, wake) = notebook();
    let cell = notebook.stream();
    cell.feed("print('first')\n".into(), false).unwrap();
    // The first statement is whole only once the next one starts.
    cell.feed("print('sec".into(), false).unwrap();
    until(&wake, || cell.progress().settled > 0).await;
    assert_eq!(cell.progress().settled, "print('first')\n".len());
    assert!(cell.facts().returned.is_none());
    cell.feed("ond')\n".into(), true).unwrap();
    finished(&wake, &cell).await;
    assert_eq!(notebook.report().unwrap().text, "first\nsecond");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_stream_keeps_what_ran() {
    let (notebook, wake) = notebook();
    let cell = notebook.stream();
    cell.feed("a = 1\nb = ".into(), false).unwrap();
    until(&wake, || cell.progress().settled > 0).await;
    assert_eq!(cell.interrupt(), Some("a = 1\n".len()));
    finished(&wake, &cell).await;
    let next = notebook.run("print(a, 'b' in globals())".into());
    finished(&wake, &next).await;
    let text = notebook.report().unwrap().text;
    assert!(text.ends_with("1 False"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_reports_under_its_session_id() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("job = command('read line; echo got $line')".into());
    finished(&wake, &cell).await;
    let first = notebook.report().unwrap().text;
    assert!(
        first.starts_with("Cell finished\n\nCommand running with session ID "),
        "{first}"
    );
    let session = first.rsplit(' ').next().unwrap().to_owned();

    let write = notebook.run("write_stdin(job, 'hi\\n')".into());
    until(&wake, || {
        notebook
            .facts()
            .iter()
            .any(|f| f.kind == crate::Kind::Command && f.finished.is_some())
    })
    .await;
    finished(&wake, &write).await;
    let text = notebook.report().unwrap().text;
    assert_eq!(
        text,
        format!(
            "Session ID: {session}\nProcess exited with code 0\nOutput:\ngot hi\n\nCell finished"
        )
    );
    // A command stays to be paged.
    let page = notebook.run(format!("Command.from_session_id({session}).more_output()"));
    finished(&wake, &page).await;
    let text = notebook.report().unwrap().text;
    assert!(
        text.starts_with(&format!(
            "Session ID: {session}\nCommand: read line; echo got $line\nNo more output."
        )),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_task_outlives_its_return() {
    let (notebook, wake) = notebook();
    let cell = notebook.run(
        "import asyncio\nasync def later():\n    await asyncio.sleep(0.2)\n    print('late')\nasyncio.create_task(later())\nprint('now')".into(),
    );
    until(&wake, || cell.facts().returned.is_some()).await;
    let text = notebook.report().unwrap().text;
    assert_eq!(
        text,
        format!(
            "Cell running with session ID {}\nOutput:\nnow",
            cell.session_id()
        )
    );
    finished(&wake, &cell).await;
    assert_eq!(
        notebook.report().unwrap().text,
        format!(
            "Session ID: {}\nCell finished\nOutput:\nlate",
            cell.session_id()
        )
    );
}
