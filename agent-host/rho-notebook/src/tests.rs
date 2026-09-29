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
async fn selected_ghapi_sources_are_importable_in_the_notebook() {
    let (notebook, wake) = notebook();
    let cell = notebook.run(
        r#"import ghapi, sys
from ghapi.all import GhApi
from ghapi.core import CheckRun
assert ghapi.__file__.startswith(sys.path[1] + "/ghapi/"), ghapi.__file__
assert GhApi.__module__ == "ghapi.core"
assert CheckRun(id=8, name="build", status="completed", conclusion="success",
                started_at=None, completed_at=None).name == "build"
print("ghapi import ready")"#
            .into(),
    );
    finished(&wake, &cell).await;
    assert_eq!(
        notebook.report().unwrap().render().text,
        "ghapi import ready"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn python_ls_xdir_is_available_in_the_notebook() {
    let (notebook, wake) = notebook();
    let cell = notebook.run(
        r#"from python_ls import xdir
class Example:
    @property
    def token(self):
        raise RuntimeError("inspecting a property must not execute it")
assert xdir(Example(), "token") == ["token"]
assert xdir({"status": {"failure_code": 3}, "state": "pending"},
            "fail", depth=2) == ["['status']['failure_code']"]
print("xdir ready")"#
            .into(),
    );
    finished(&wake, &cell).await;
    assert_eq!(notebook.report().unwrap().render().text, "xdir ready");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cell_that_ends_first_speaks_plainly() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("print(6 * 7)".into());
    finished(&wake, &cell).await;
    assert_eq!(notebook.report().unwrap().render().text, "42");
    assert!(notebook.report().is_none());
    assert!(notebook.facts().is_empty());

    let silent = notebook.run("x = 1".into());
    finished(&wake, &silent).await;
    assert_eq!(notebook.report().unwrap().render().text, "Task finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_raise_is_the_cells_output() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("raise ValueError('nope')".into());
    finished(&wake, &cell).await;
    assert!(cell.facts().finished.unwrap().failed);
    assert!(
        notebook
            .report()
            .unwrap()
            .render()
            .text
            .contains("ValueError: nope")
    );
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
    assert_eq!(notebook.report().unwrap().render().text, "first\nsecond");
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
    let text = notebook.report().unwrap().render().text;
    assert!(text.starts_with("1 False"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_implicitly_holds_its_task_and_can_be_paged() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("job = command('read line; echo got $line')".into());
    until(&wake, || cell.facts().returned.is_some()).await;
    assert!(cell.facts().finished.is_none());
    let first = notebook.report().unwrap().render().text;
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
    let text = notebook.report().unwrap().render().text;
    assert!(
        text.contains(&format!(
            "Session ID: {session}\nProcess exited with code 0\nOutput:\ngot hi"
        )),
        "{text}"
    );
    let page = notebook.run(format!("Command.from_session_id({session}).more_output()"));
    finished(&wake, &page).await;
    let text = notebook.report().unwrap().render().text;
    assert!(text.contains("No more output."), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_task_outlives_its_return() {
    let (notebook, wake) = notebook();
    let cell = notebook.run(
        "import asyncio\nasync def later():\n    await asyncio.sleep(0.2)\n    print('late')\nasyncio.create_task(later())\nprint('now')".into(),
    );
    until(&wake, || cell.facts().returned.is_some()).await;
    let text = notebook.report().unwrap().render().text;
    assert!(text.contains("now"), "{text}");
    assert!(
        text.contains("Task running in background with session ID"),
        "{text}"
    );
    finished(&wake, &cell).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        notebook
            .report()
            .unwrap()
            .render()
            .text
            .contains("Output:\nlate")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn created_tasks_own_output_and_return_values() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("import asyncio\nasync def fetch():\n    await asyncio.sleep(0.1)\n    print('child')\n    return 73\nt = asyncio.create_task(fetch())\nprint('parent')".into());
    finished(&wake, &cell).await;
    let first = notebook.report().unwrap().render().text;
    assert!(first.contains("parent"), "{first}");
    assert!(!first.contains("child"), "{first}");
    let next = notebook.run("print(await t)".into());
    finished(&wake, &next).await;
    let second = notebook.report().unwrap().render().text;
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
    let text = notebook.report().unwrap().render().text;
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
    let text = notebook.report().unwrap().render().text;
    assert!(text.contains("Task cancelled"), "{text}");
    assert!(
        !text.contains("Task failed") && !text.contains("Command failed"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caught_created_task_exception_is_not_reported() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("import asyncio\nasync def broken():\n    raise ValueError('specific')\nt = asyncio.create_task(broken())\ntry:\n    await t\nexcept ValueError:\n    print('caught')".into());
    finished(&wake, &cell).await;
    let text = notebook.report().unwrap().render().text;
    assert!(text.contains("caught"), "{text}");
    assert!(!text.contains("ValueError: specific"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn output_keeps_four_megabyte_head_and_notes_the_cut() {
    let (notebook, wake) = notebook();
    let cell = notebook
        .run("for _ in range(110): print('a' * 40000, max_tokens=10000)\nprint('b' * 100)".into());
    finished(&wake, &cell).await;
    let text = notebook.report().unwrap().render().text;
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
    let text = notebook.report().unwrap().render().text;
    assert!(text.contains("retained output is gone"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unclaimed_failure_reports_once_after_twenty_seconds() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("import asyncio\nasync def broken():\n    raise ValueError('unclaimed')\nt = asyncio.create_task(broken())".into());
    finished(&wake, &cell).await;
    assert!(
        !notebook
            .report()
            .unwrap()
            .render()
            .text
            .contains("unclaimed")
    );
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
    let text = notebook.report().unwrap().render().text;
    assert!(
        text.contains("Task failed\nValueError: unclaimed"),
        "{text}"
    );
    assert!(notebook.report().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_a_failed_commands_exit_code_takes_its_failure() {
    let (notebook, wake) = notebook();
    let failed = || {
        notebook
            .facts()
            .iter()
            .any(|f| f.kind == crate::Kind::Command && f.finished.is_some_and(|end| end.failed))
    };
    // Awaited but its exit code unread: still a failure.
    let cell = notebook.run("exit = await command('exit 3')\nprint(exit)".into());
    finished(&wake, &cell).await;
    assert!(failed());
    let text = notebook.report().unwrap().render().text;
    assert!(
        text.contains("CommandExit(id=") && text.contains("exit_code=3)"),
        "{text}"
    );
    assert!(!failed(), "a delivered failure is spent");

    // A watcher that reads the exit code handles it itself.
    let cell = notebook.run(
        "import asyncio\nasync def watch():\n    if (await command('exit 4')).exit_code != 0:\n        print('down')\nt = asyncio.create_task(watch())"
            .into(),
    );
    finished(&wake, &cell).await;
    until(&wake, || {
        notebook
            .facts()
            .iter()
            .any(|f| f.kind == crate::Kind::Command && f.finished.is_some())
    })
    .await;
    until(&wake, || {
        notebook.facts().iter().any(|f| f.output_since.is_some())
    })
    .await;
    assert!(!failed());
    assert!(notebook.report().unwrap().render().text.contains("down"));
}

#[tokio::test(flavor = "multi_thread")]
async fn callbacks_and_threads_do_not_hold_a_task_but_keep_its_output() {
    let (notebook, wake) = notebook();
    let cell = notebook.run(
        "import asyncio, threading\ncallback_gate = asyncio.get_running_loop().create_future()\ncallback_gate.add_done_callback(lambda _: print('callback'))\nthread_gate = threading.Event()\nthreading.Thread(target=lambda: (thread_gate.wait(), print('thread'))).start()".into(),
    );
    finished(&wake, &cell).await;
    assert_eq!(notebook.report().unwrap().render().text, "Task finished");

    // Release both only after the original task's end has been reported.
    // A fixed delay lets a loaded test runner observe the callback too early.
    let release = notebook.run("callback_gate.set_result(None)\nthread_gate.set()".into());
    finished(&wake, &release).await;
    let mut report = crate::Report::default();
    until(&wake, || {
        if let Some(update) = notebook.report() {
            report.merge(update);
        }
        let text = report.render().text;
        text.contains("callback") && text.contains("thread")
    })
    .await;
    let text = report.render().text;
    let (_, original) = text
        .split_once(&format!("Session ID: {}\n", cell.session_id()))
        .expect("late output retains the original cell's session");
    // Output chunks can themselves contain blank lines; only a new session
    // header starts another source.
    let original = original.split("\n\nSession ID: ").next().unwrap();
    assert!(original.contains("callback"), "{text}");
    assert!(original.contains("thread"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_handle_and_await_result_use_scrambled_session_id() {
    let (notebook, wake) = notebook();
    let cell = notebook.run("job = command('exit 7')\nprint(job.id, (await job).id)".into());
    finished(&wake, &cell).await;
    let command = notebook
        .facts()
        .iter()
        .find(|f| f.kind == crate::Kind::Command)
        .unwrap()
        .session_id;
    let text = notebook.report().unwrap().render().text;
    assert!(text.contains(&format!("{command} {command}")), "{text}");
    assert!(!text.contains("2 2"), "raw internal ID leaked: {text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn checkin_remains_configurable_without_tool_suppression() {
    let (notebook, wake) = notebook();
    assert_eq!(notebook.checkin(), Duration::from_secs(120));
    let cell =
        notebook.run("print('suppress_tool_wakeups' in globals())\nset_max_wait(317)".into());
    finished(&wake, &cell).await;
    assert_eq!(notebook.report().unwrap().render().text, "False");
    assert_eq!(notebook.checkin(), Duration::from_secs(317));
    notebook.reset_checkin();
    assert_eq!(notebook.checkin(), Duration::from_secs(120));
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_from_new_notebooks_with_reused_session_labels_stay_separate() {
    let (first, wake) = notebook();
    let cell = first.run("print('first lifetime')".into());
    finished(&wake, &cell).await;
    let label = cell.session_id();
    let mut report = first.report().unwrap();
    drop(first);

    let (second, wake) = notebook();
    let cell = second.run("print('second lifetime')".into());
    finished(&wake, &cell).await;
    assert_eq!(cell.session_id(), label);
    report.merge(second.report().unwrap());
    assert_eq!(report.render().text, "first lifetime\n\nsecond lifetime");
}
