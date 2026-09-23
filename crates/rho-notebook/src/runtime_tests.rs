//! The runtime underneath the boundary: asyncio ownership, streaming
//! units, output routing and interpreter isolation.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pyo3::prelude::*;
use rho_core::{ExecCall, ToolOutput, ToolOutputStatus};
use tokio::sync::Notify;

use crate::tests::{shell, shell_in};
use crate::{Export, PythonCell, PythonNotebook, SourceWaker};

fn notebook() -> PythonNotebook {
    PythonNotebook::new(shell(), Vec::new()).unwrap()
}

fn start(notebook: &PythonNotebook, source: &str) -> (Box<PythonCell>, Arc<Notify>) {
    let wake = Arc::new(Notify::new());
    let cell = notebook.exec(
        ExecCall {
            id: "cell".try_into().unwrap(),
            source: source.into(),
        },
        SourceWaker::new(wake.clone()),
    );
    (cell, wake)
}

async fn until(wake: &Notify, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !done() {
            let _ = tokio::time::timeout(Duration::from_millis(100), wake.notified()).await;
        }
    })
    .await
    .expect("notebook timed out");
}

/// Everything the cell said once it and its work have ended.
async fn finish(mut cell: Box<PythonCell>, wake: &Notify) -> ToolOutput {
    let exec = cell.execution();
    until(wake, || exec.quiescent()).await;
    let output = cell.first_output();
    cell.acknowledge_output();
    output
}

async fn run(notebook: &PythonNotebook, source: &str) -> ToolOutput {
    let (cell, wake) = start(notebook, source);
    finish(cell, &wake).await
}

async fn run_ok(notebook: &PythonNotebook, source: &str) -> String {
    let output = run(notebook, source).await;
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    output.output.as_str().to_owned()
}

/// Stream `source` to a new cell, admitting each unit as it becomes ready.
async fn stream(notebook: &PythonNotebook, source: &str) -> ToolOutput {
    let wake = Arc::new(Notify::new());
    let cell = notebook.start_stream("stream".try_into().unwrap(), SourceWaker::new(wake.clone()));
    let exec = cell.execution();
    exec.feed(source.into(), true).unwrap();
    until(&wake, || {
        exec.admit_stream_unit().unwrap();
        exec.quiescent()
    })
    .await;
    finish(cell, &wake).await
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_loads_constant_sets_and_preserves_notebook_state() {
    let notebook = notebook();
    for source in [
        "names = {'fedimint-core', 'fedimint-api-client', 'fedimint-wallet-client'}",
        "assert sorted(names) == ['fedimint-api-client', 'fedimint-core', 'fedimint-wallet-client']\nnames.add('extra')\nassert len(names) == 4\ndef nested(value):\n return value in {'fedimint-core', 'fedimint-api-client', 'fedimint-wallet-client'}\nassert nested('fedimint-core') and not nested('extra')\nassert eval(\"{'a', 'b', 'c'}\") == set(['a', 'b', 'c'])",
    ] {
        let output = stream(&notebook, source).await;
        assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    }
}

/// `echo(text, *, count=1)`, `broken()` and `detached()`, as a host
/// would write them.
#[pyclass(frozen)]
struct Tools {
    detached: Arc<AtomicUsize>,
}

#[pymethods]
impl Tools {
    #[pyo3(signature = (text, *, count = 1))]
    fn echo(&self, py: Python<'_>, text: String, count: u64) -> PyResult<Py<PyAny>> {
        crate::operation(py, "echo", move |_| async move { Ok((text, count)) })
    }

    fn broken(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        crate::operation(py, "broken", |_| async {
            Err::<(), _>("host failed".to_owned())
        })
    }

    fn detached(&self, py: Python<'_>) -> PyResult<()> {
        let detached = Arc::clone(&self.detached);
        crate::detached(py, "detached", move |_| async move {
            detached.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_tools_take_python_arguments_and_return_python_values() {
    let detached = Arc::new(AtomicUsize::new(0));
    let notebook = PythonNotebook::new(
        shell(),
        vec![Export::new(
            "tools",
            Tools {
                detached: detached.clone(),
            },
        )],
    )
    .unwrap();
    let output = run(
        &notebook,
        r#"
assert await tools.echo('hi', count=3) == ('hi', 3)
from tools import echo
assert await echo(text='hi') == ('hi', 1)
async def rejects():
    for call in [lambda: echo('a', 'b'), lambda: echo(text='a', extra=1), lambda: echo(1)]:
        try:
            call()
        except TypeError:
            pass
        else: raise AssertionError('bad arguments accepted')
    try:
        await tools.broken()
    except RuntimeError as error:
        assert "host failed" in str(error)
    else: raise AssertionError('host failure')
await rejects()
assert tools.detached() is None
print('checked')
"#,
    )
    .await;
    // A failed operation is the cell's failure too.
    assert_eq!(output.status, ToolOutputStatus::Error, "{output:?}");
    assert!(output.output.starts_with("checked\n"), "{output:?}");
    assert!(
        output
            .output
            .contains("Operation broken completed\nOutput:\nhost failed"),
        "{output:?}"
    );
    assert_eq!(detached.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_start_await_and_control_real_processes() {
    let directory = tempfile::tempdir().unwrap();
    let notebook = notebook();
    let source = format!(
        r#"
job = command('read line; echo "got $line"; pwd', workdir={:?}, max_tokens=12000)
await write_stdin(job, 'input\n')
exit = await job
assert exit == {{'id': job.id, 'exit_code': 0}}, exit
assert await job == exit
await job.more_output(max_tokens=12000)
await job.cancel()
for bad in [lambda: command(1), lambda: write_stdin('job', 'x')]:
    try: bad()
    except TypeError: pass
    else: raise AssertionError('bad arguments accepted')
try: job.more_output(max_tokens=0)
except ValueError: pass
else: raise AssertionError('bad budget accepted')
assert repr(job) == f'<command {{job.id}}>'
"#,
        directory.path().to_str().unwrap()
    );
    let output = run_ok(&notebook, &source).await;
    assert!(output.contains("got input"), "{output}");
    assert!(
        output.contains(directory.path().to_str().unwrap()),
        "{output}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn input_backlog_crosses_multiple_drain_batches_without_refusing_a_cell() {
    let notebook = notebook();
    let wake = Arc::new(Notify::new());
    let backlog = notebook.start_stream(
        "backlog".try_into().unwrap(),
        SourceWaker::new(wake.clone()),
    );
    for _ in 0..513 {
        backlog.stop_stream();
    }
    let output = run_ok(&notebook, "notify('after the backlog')").await;
    assert!(output.contains("after the backlog"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_bypasses_backlog_and_ends_waiting_cells() {
    let notebook = notebook();
    let (cell, wake) = start(&notebook, "notify('waiting'); await asyncio.Event().wait()");
    let exec = cell.execution();
    until(&wake, || exec.facts().notified_at.is_some()).await;
    let backlog = notebook.start_stream(
        "backlog".try_into().unwrap(),
        SourceWaker::new(wake.clone()),
    );
    for _ in 0..513 {
        backlog.stop_stream();
    }
    drop(notebook);
    let output = finish(cell, &wake).await;
    assert_ne!(output.status, ToolOutputStatus::Success, "{output:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_requires_each_permit_and_stop_does_not_finish_the_suffix() {
    let notebook = notebook();
    let wake = Arc::new(Notify::new());
    let cell = notebook.start_stream("stream".try_into().unwrap(), SourceWaker::new(wake.clone()));
    let exec = cell.execution();
    let first = "seen = ['first']\n";
    exec.feed(
        format!("{first}seen.append('second')\nif True:\n    seen.append('suffix')\n"),
        false,
    )
    .unwrap();
    until(&wake, || exec.stream_progress().ready == Some(first.len())).await;
    run_ok(&notebook, "assert 'seen' not in globals()").await;
    exec.admit_stream_unit().unwrap();
    until(&wake, || {
        let progress = exec.stream_progress();
        progress.completed == first.len() && progress.ready > Some(first.len())
    })
    .await;
    exec.stop_stream();
    let output = finish(cell, &wake).await;
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    run_ok(&notebook, "assert seen == ['first']").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_eof_closes_compounds_and_preserves_future_flags() {
    let notebook = notebook();
    let output = stream(
        &notebook,
        "from __future__ import annotations\nif True:\n    def f(x: Missing):\n        return x\n",
    )
    .await;
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    run_ok(&notebook, "assert f.__annotations__['x'] == 'Missing'").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_loss_allows_admitted_await_to_settle_without_admitting_more() {
    let notebook = notebook();
    run_ok(
        &notebook,
        "import asyncio\ngate = asyncio.Event()\nseen = []",
    )
    .await;
    let wake = Arc::new(Notify::new());
    let cell = notebook.start_stream("stream".try_into().unwrap(), SourceWaker::new(wake.clone()));
    let exec = cell.execution();
    let first = "await gate.wait(); seen.append('settled')\n";
    exec.feed(format!("{first}seen.append('wrong')\n"), false)
        .unwrap();
    until(&wake, || exec.stream_progress().ready.is_some()).await;
    exec.admit_stream_unit().unwrap();
    exec.stop_stream();
    run_ok(&notebook, "gate.set()").await;
    let output = finish(cell, &wake).await;
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    let progress = exec.stream_progress();
    assert_eq!(
        (progress.completed, progress.admitted),
        (first.len(), first.len())
    );
    run_ok(&notebook, "assert seen == ['settled']").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_libraries_and_bundled_packages_work() {
    run_ok(&notebook(), r#"
import ssl, sqlite3, yaml, httpx, pickle, threading, time
assert ssl.create_default_context().check_hostname
with sqlite3.connect(':memory:') as database:
 database.execute('create table values_ (value text)')
 database.execute('insert into values_ values (?)', ('雪',))
 assert database.execute('select value from values_').fetchone() == ('雪',)
assert yaml.safe_load('items: [one, two]') == {'items': ['one', 'two']}
with httpx.Client(transport=httpx.MockTransport(lambda request: httpx.Response(200, json={'path': request.url.path}))) as client:
 assert client.get('https://test.invalid/example').json() == {'path': '/example'}
assert isinstance(pickle.loads(pickle.dumps({'snow': '雪'})), dict)
assert threading.get_ident() != await asyncio.to_thread(threading.get_ident)
events = []
async def tick(): await asyncio.sleep(.02); events.append('tick')
async def work(): await asyncio.to_thread(time.sleep, .05); events.append('work')
await asyncio.gather(tick(), work())
assert events == ['tick', 'work']
"#).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn default_text_io_uses_utf8() {
    run_ok(&notebook(), r#"
import io, sys, tempfile
assert sys.flags.utf8_mode == 1 and io.text_encoding(None) == 'utf-8'
sample = '— 雪🙂 café'
with io.TextIOWrapper(io.BytesIO(sample.encode('utf-8'))) as wrapper: assert wrapper.read() == sample
with tempfile.TemporaryDirectory() as directory:
 path = Path(directory) / 'unicode.txt'
 path.write_text(sample)
 assert path.read_bytes() == sample.encode('utf-8')
"#).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn live_cells_share_globals_and_keep_attribution() {
    let notebook = notebook();
    let (first, first_wake) = start(
        &notebook,
        "values = []\nvalues.append(1)\nawait asyncio.sleep(.05)\nvalues.append(3)\nnotify(values)",
    );
    let second = run_ok(&notebook, "values.append(2)\nprint(values)").await;
    assert_eq!(second, "[1, 2]\n");
    let first = finish(first, &first_wake).await;
    assert_eq!(first.output.as_str(), "[1, 2, 3]\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn output_limits_and_wait_controls() {
    let notebook = notebook();
    let (cell, wake) = start(
        &notebook,
        r#"
try: print('x' * (1024 * 1024 + 1), max_tokens=1)
except Exception as error: raise AssertionError(repr(error))
print('Ω "quoted"')
set_max_wait(seconds=3)
suppress_tool_wakeups()
"#,
    );
    let exec = cell.execution();
    let output = finish(cell, &wake).await;
    assert_eq!(output.output.as_str(), "xxxx\n[truncated]Ω \"quoted\"\n");
    let checkin = exec.facts().checkin.unwrap();
    assert_eq!(checkin.after, Duration::from_secs(3));
    assert!(!checkin.wake_on_tools);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_stops_awaiting_work_without_losing_notebook() {
    let notebook = notebook();
    let (mut cell, wake) = start(
        &notebook,
        "survives = 42\nasyncio.create_task(asyncio.sleep(3600))\nnotify('started')\nawait asyncio.Event().wait()",
    );
    let exec = cell.execution();
    until(&wake, || exec.facts().notified_at.is_some()).await;
    cell.cancel();
    let output = finish(cell, &wake).await;
    assert_eq!(output.status, ToolOutputStatus::Cancelled, "{output:?}");
    assert!(output.output.contains("CancelledError"), "{output:?}");
    assert_eq!(run_ok(&notebook, "print(survives)").await, "42\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn other_asyncio_loops_run_in_their_own_threads() {
    run_ok(
        &notebook(),
        r#"
import threading
assert type(asyncio.get_running_loop()).__name__ == 'NotebookEventLoop'
async def work(n):
 await asyncio.sleep(.01)
 return n * 2
results = []
thread = threading.Thread(target=lambda: results.append(asyncio.run(work(21))))
thread.start()
await asyncio.to_thread(thread.join)
assert results == [42], results
def reuse():
 loop = asyncio.new_event_loop()
 try:
  ticks = []
  async def background():
   while True:
    ticks.append(1)
    await asyncio.sleep(0)
  task = loop.create_task(background())
  assert loop.run_until_complete(work(1)) == 2
  seen = len(ticks)
  assert loop.run_until_complete(work(2)) == 4
  assert len(ticks) > seen, 'background task stalled between runs'
  task.cancel()
  loop.run_until_complete(asyncio.sleep(0))
  assert task.cancelled()
  loop.call_later(.01, loop.stop)
  loop.run_forever()
 finally:
  loop.close()
await asyncio.to_thread(reuse)
"#,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gathered_errors_can_be_handled_and_child_tasks_stay_attributed() {
    let output = run_ok(&notebook(), "async def child():\n await asyncio.sleep(.01)\n notify('child')\n raise ValueError('expected')\nresults = await asyncio.gather(child(), return_exceptions=True)\nassert isinstance(results[0], ValueError)").await;
    assert_eq!(output, "child\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_asyncio_queues_task_groups_timeouts_and_streams_work() {
    run_ok(
        &notebook(),
        r#"
queue = asyncio.Queue(maxsize=1)
lock, values = asyncio.Lock(), []
async def produce():
 for n in range(3): await queue.put(n)
async def consume():
 for _ in range(3):
  n = await queue.get()
  async with lock: values.append(n)
  queue.task_done()
async with asyncio.TaskGroup() as group:
 group.create_task(produce()); group.create_task(consume())
await queue.join()
assert values == [0, 1, 2]
for use_context in [True, False]:
 try:
  if use_context:
   async with asyncio.timeout(.01): await asyncio.Event().wait()
  else: await asyncio.wait_for(asyncio.Event().wait(), .01)
 except TimeoutError: pass
 else: raise AssertionError('timeout did not fire')
async def echo(reader, writer):
 writer.write((await reader.readline()).upper()); await writer.drain(); writer.close()
server = await asyncio.start_server(echo, '127.0.0.1', 0)
async with server:
 reader, writer = await asyncio.open_connection('localhost', server.sockets[0].getsockname()[1])
 writer.write(b'hello\n'); await writer.drain()
 assert await reader.readline() == b'HELLO\n'
 writer.close()
"#,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_asyncio_thread_work_preserves_cell_context() {
    let output = run_ok(
        &notebook(),
        "await asyncio.to_thread(notify, 'thread output')",
    )
    .await;
    assert_eq!(output, "thread output\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_asyncio_callbacks_keep_cells_alive_and_can_be_cancelled() {
    let notebook = notebook();
    let output = run_ok(
        &notebook,
        "asyncio.get_running_loop().call_later(.01, notify, 'callback')",
    )
    .await;
    assert_eq!(output, "callback\n");
    let (mut cell, wake) = start(
        &notebook,
        "asyncio.get_running_loop().call_later(3600, notify, 'too late')\nprint('scheduled')",
    );
    let exec = cell.execution();
    until(&wake, || exec.facts().returned.is_some()).await;
    assert!(!exec.quiescent());
    cell.cancel();
    let output = finish(cell, &wake).await;
    assert!(output.output.contains("CancelledError"), "{output:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn selector_callbacks_keep_returned_cells_alive_and_print_errors() {
    let notebook = notebook();
    let (watching, wake) = start(
        &notebook,
        r#"
import socket
reader, writer = socket.socketpair()
loop = asyncio.get_running_loop()
def ready():
 reader.recv(1); loop.remove_reader(reader); reader.close(); raise ValueError('reader failure')
loop.add_reader(reader, ready)
print('watching')
"#,
    );
    let exec = watching.execution();
    until(&wake, || exec.facts().returned.is_some()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!exec.quiescent());
    run_ok(&notebook, "writer.send(b'x'); writer.close()").await;
    let output = finish(watching, &wake).await;
    assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    assert!(output.output.contains("watching"), "{output:?}");
    assert!(
        output.output.contains("ValueError: reader failure"),
        "{output:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_streams_write_to_the_cell() {
    let output = run_ok(&notebook(), "import sys\nprint('hello', 'world', end='!', max_tokens=12000)\nsys.stderr.write('diagnostic')").await;
    assert_eq!(output, "hello world!diagnostic");
}

#[tokio::test(flavor = "multi_thread")]
async fn pathlib_is_prebound_and_each_notebook_has_its_own_cwd() {
    let original = std::env::current_dir().unwrap();
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    let first = PythonNotebook::new(shell_in(&one), Vec::new()).unwrap();
    let second = PythonNotebook::new(shell_in(&two), Vec::new()).unwrap();
    let (a, a_wake) = start(
        &first,
        "import os\nPath('sub').mkdir()\nos.chdir('sub')\nPath('note.txt').write_text('one')\nassert Path('note.txt').read_text() == 'one'",
    );
    let (b, b_wake) = start(
        &second,
        "assert pathlib.Path is Path\nPath('note.txt').write_text('two')\nassert Path('note.txt').read_text() == 'two'",
    );
    assert_eq!(finish(a, &a_wake).await.status, ToolOutputStatus::Success);
    assert_eq!(finish(b, &b_wake).await.status, ToolOutputStatus::Success);
    assert_eq!(std::env::current_dir().unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(one.path().join("sub/note.txt")).unwrap(),
        "one"
    );
    assert_eq!(
        std::fs::read_to_string(two.path().join("note.txt")).unwrap(),
        "two"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn notebooks_have_separate_globals_and_output() {
    let first = notebook();
    let second = notebook();
    assert_eq!(
        run_ok(&first, "secret = 'first'\nprint('from first')").await,
        "from first\n"
    );
    assert_eq!(
        run_ok(
            &second,
            "assert 'secret' not in globals()\nprint('from second')"
        )
        .await,
        "from second\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn threads_keep_their_cell_alive_and_write_to_it() {
    let output = run_ok(&notebook(), "import threading, time\ndef work():\n time.sleep(.05)\n print('from thread')\nthreading.Thread(target=work).start()").await;
    assert_eq!(output, "from thread\n");
}
