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
    assert_eq!(notebook.report().unwrap().text, "Task finished");
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
    assert!(text.starts_with("1 False"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_implicitly_holds_its_task_and_can_be_paged() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("job = command('read line; echo got $line')".into());
    until(&wake, || cell.facts().returned.is_some()).await;
    assert!(cell.facts().finished.is_none());
    let first = notebook.report().unwrap().text;
    assert!(
        first.contains("Command running in background with session ID "),
        "{first}"
    );
    let session = notebook
        .facts()
        .iter()
        .find(|f| f.kind == crate::Kind::Command)
        .unwrap()
        .session_id;

    let write = notebook.run("write_stdin(job, 'hi\\n')".into());
    finished(&wake, &cell).await;
    finished(&wake, &write).await;
    let text = notebook.report().unwrap().text;
    assert!(
        text.contains(&format!(
            "Session ID: {session}\nProcess exited with code 0\nOutput:\ngot hi"
        )),
        "{text}"
    );
    let page = notebook.run(format!("Command.from_session_id({session}).more_output()"));
    finished(&wake, &page).await;
    let text = notebook.report().unwrap().text;
    assert!(text.contains("No more output."), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_task_outlives_its_return() {
    let (notebook, wake) = notebook();
    let cell = notebook.run(
        "import asyncio\nasync def later():\n    await asyncio.sleep(0.2)\n    print('late')\nasyncio.create_task(later())\nprint('now')".into(),
    );
    until(&wake, || cell.facts().returned.is_some()).await;
    let text = notebook.report().unwrap().text;
    assert!(text.contains("now"), "{text}");
    assert!(
        text.contains("Task running in background with session ID"),
        "{text}"
    );
    finished(&wake, &cell).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(notebook.report().unwrap().text.contains("Output:\nlate"));
}

#[tokio::test(flavor = "multi_thread")]
async fn created_tasks_own_output_and_return_values() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("import asyncio\nasync def fetch():\n    await asyncio.sleep(0.1)\n    print('child')\n    return 73\nt = asyncio.create_task(fetch())\nprint('parent')".into());
    finished(&wake, &cell).await;
    let first = notebook.report().unwrap().text;
    assert!(first.contains("parent"), "{first}");
    assert!(!first.contains("child"), "{first}");
    let next = notebook.run("print(await t)".into());
    finished(&wake, &next).await;
    let second = notebook.report().unwrap().text;
    assert!(second.contains("child"), "{second}");
    assert!(second.contains("73"), "{second}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_raised_task_does_not_wait_for_its_command() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("job = command('sleep 1'); raise ValueError('bad')".into());
    finished(&wake, &cell).await;
    assert!(
        notebook
            .facts()
            .iter()
            .any(|f| f.kind == crate::Kind::Command && f.finished.is_none())
    );
    let text = notebook.report().unwrap().text;
    assert!(text.contains("Task failed"), "{text}");
    assert!(text.contains("ValueError: bad"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_task_kills_its_command_quietly() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("job = command('sleep 5')\nawait job".into());
    until(&wake, || {
        notebook
            .facts()
            .iter()
            .any(|f| f.kind == crate::Kind::Command)
    })
    .await;
    cell.cancel();
    finished(&wake, &cell).await;
    until(&wake, || {
        notebook
            .facts()
            .iter()
            .any(|f| f.kind == crate::Kind::Command && f.finished.is_some())
    })
    .await;
    assert!(
        !notebook
            .facts()
            .iter()
            .any(|f| f.kind == crate::Kind::Command && f.finished.unwrap().failed)
    );
    let text = notebook.report().unwrap().text;
    assert!(text.contains("Task cancelled"), "{text}");
    assert!(!text.contains("Task failed"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caught_created_task_exception_is_not_reported() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("import asyncio\nasync def broken():\n    raise ValueError('specific')\nt = asyncio.create_task(broken())\ntry:\n    await t\nexcept ValueError:\n    print('caught')".into());
    finished(&wake, &cell).await;
    let text = notebook.report().unwrap().text;
    assert!(text.contains("caught"), "{text}");
    assert!(!text.contains("ValueError: specific"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn output_keeps_four_megabyte_head_and_notes_the_cut() {
    let (notebook, wake) = notebook();
    let cell = notebook
        .run("for _ in range(110): print('a' * 40000, max_tokens=10000)\nprint('b' * 100)".into());
    finished(&wake, &cell).await;
    let text = notebook.report().unwrap().text;
    assert!(
        text.contains("[output past 4 MB was dropped]"),
        "missing cut note"
    );
    assert!(!text.contains("b".repeat(100).as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn notebook_retention_discards_oldest_command_log() {
    let (notebook, wake) = notebook();
    let cell =
        notebook.run("jobs = [command('head -c 4194304 /dev/zero') for _ in range(14)]".into());
    finished(&wake, &cell).await;
    let oldest = notebook
        .facts()
        .iter()
        .find(|f| f.kind == crate::Kind::Command)
        .unwrap()
        .session_id;
    let _ = notebook.report();
    let page = notebook.run(format!("Command.from_session_id({oldest}).more_output()"));
    finished(&wake, &page).await;
    let text = notebook.report().unwrap().text;
    assert!(text.contains("retained output is gone"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unclaimed_failure_reports_once_after_twenty_seconds() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("import asyncio\nasync def broken():\n    raise ValueError('unclaimed')\nt = asyncio.create_task(broken())".into());
    finished(&wake, &cell).await;
    assert!(!notebook.report().unwrap().text.contains("unclaimed"));
    tokio::time::timeout(Duration::from_secs(23), async {
        loop {
            wake.notified().await;
            if notebook
                .facts()
                .iter()
                .any(|f| f.kind == crate::Kind::Task && f.finished.is_some_and(|end| end.failed))
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let text = notebook.report().unwrap().text;
    assert!(
        text.contains("Task failed\nValueError: unclaimed"),
        "{text}"
    );
    assert!(notebook.report().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn callbacks_and_threads_do_not_hold_a_task_but_keep_its_output() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("import asyncio, threading, time\nasyncio.get_running_loop().call_later(0.1, lambda: print('callback'))\nthreading.Thread(target=lambda: (time.sleep(0.1), print('thread'))).start()".into());
    finished(&wake, &cell).await;
    assert_eq!(notebook.report().unwrap().text, "Task finished");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let text = notebook.report().unwrap().text;
    assert!(text.contains("callback"), "{text}");
    assert!(text.contains("thread"), "{text}");
    assert!(
        text.starts_with(&format!("Session ID: {}", cell.session_id())),
        "{text}"
    );
}
