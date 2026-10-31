"""What Rust calls: the workset thread's start, and thread-safe entries
onto its asyncio loop.

`_main` runs on a thread of its own at process start and owns the workset
loop until the process ends. `_eval` and `_hook` are called from other
threads and run their work on that loop. `_on_notebook` runs on the thread
of the agent notebook that starts.
"""

import ast
import asyncio
import contextvars
import inspect
import io
import itertools
import linecache
import os
import sys
import threading
import traceback

import _rho

from . import agents, default, hooks, workset

_loop: asyncio.AbstractEventLoop | None = None
_thread: int | None = None
#: Agents `hooks.on_agent` ran for, so each gets it once.
_announced: set[str] = set()
#: Where the running `_eval`'s output goes; unset elsewhere.
_output: contextvars.ContextVar = contextvars.ContextVar("rho_eval_output", default=None)
_evals = itertools.count(1)


def _main(init_path, ready) -> None:
    """Install the default policy, run init.py, report through `ready`, then
    run the workset loop forever. A failing init.py leaves the default
    policy, and `ready` gets its traceback."""
    global _loop, _thread
    _loop = asyncio.new_event_loop()
    asyncio.set_event_loop(_loop)
    _thread = threading.get_ident()
    default.install()
    error = None
    if init_path is not None and os.path.exists(init_path):
        saved_hooks = {name: getattr(hooks, name) for name in hooks.NAMES}
        saved_namespace = dict(workset.namespace)
        with open(init_path, encoding="utf-8") as file:
            source = file.read()
        error = _loop.run_until_complete(_run(source, init_path))
        if error is not None:
            for name, hook in saved_hooks.items():
                setattr(hooks, name, hook)
            workset.namespace.clear()
            workset.namespace.update(saved_namespace)
    for id in _rho.agents():
        _announce(id)
    ready(error)
    _loop.run_forever()


def _eval(code: str) -> tuple[str, str | None]:
    """Run `code` in the workset namespace on the workset loop, top-level
    `await` allowed. Returns what it printed and its traceback, if any."""
    _ensure_off_loop("_eval")

    async def run():
        buffer = io.StringIO()
        _output.set(buffer)
        error = await _run(code, f"<eval-{next(_evals)}>")
        return buffer.getvalue(), error

    _route_output()
    return asyncio.run_coroutine_threadsafe(run(), _loop).result()


def _hook(name: str, *args, timeout: float):
    """Call `hooks.<name>` on the workset loop with the agent id first
    argument made a `rho.Agent`, await it if it is async, and return its
    result or raise. Raises TimeoutError after `timeout` seconds."""
    _ensure_off_loop("_hook")
    if name not in hooks.NAMES:
        raise AttributeError(f"no hook {name}")

    async def call():
        hook = getattr(hooks, name)
        result = hook(agents.Agent(args[0]), *args[1:])
        if inspect.isawaitable(result):
            result = await asyncio.wait_for(result, timeout)
        return result

    future = asyncio.run_coroutine_threadsafe(call(), _loop)
    try:
        return future.result(timeout)
    except TimeoutError:
        future.cancel()
        raise


def _on_notebook(agent_id: str, ns: dict) -> None:
    """Call `hooks.on_notebook` on this thread, the agent notebook's."""
    result = hooks.on_notebook(agents.Agent(agent_id), ns)
    if inspect.isawaitable(result):
        # This thread's loop, if it has one, is not running yet; leave it be.
        loop = asyncio.new_event_loop()
        try:
            loop.run_until_complete(result)
        finally:
            loop.close()


def announce_soon(agent_id: str) -> None:
    """Run `hooks.on_agent` for a new agent on the workset loop, from any
    thread."""
    _loop.call_soon_threadsafe(_announce, agent_id)


def _announce(agent_id: str) -> None:
    if agent_id in _announced:
        return
    _announced.add(agent_id)
    try:
        result = hooks.on_agent(agents.Agent(agent_id))
        if inspect.isawaitable(result):
            spawn(result)
    except Exception:
        traceback.print_exc()


_tasks: set[asyncio.Task] = set()


def spawn(coro) -> asyncio.Task:
    """A task on the workset loop that is kept until it ends; its failure is
    printed to the process's stderr."""
    task = _loop.create_task(coro)
    _tasks.add(task)

    def done(task):
        _tasks.discard(task)
        if not task.cancelled() and task.exception() is not None:
            traceback.print_exception(task.exception(), file=sys.stderr)

    task.add_done_callback(done)
    return task


async def _run(source: str, filename: str) -> str | None:
    """Run `source` in the workset namespace; the traceback, or None."""
    linecache.cache[filename] = (len(source), None, source.splitlines(True), filename)
    try:
        code = compile(source, filename, "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT, dont_inherit=True)
        result = eval(code, workset.namespace)
        if inspect.isawaitable(result):
            await result
    except asyncio.CancelledError:
        raise
    except BaseException as error:
        # SystemExit too: code run here must not end the workset loop.
        # Leave out this frame: the traceback starts in the user's code.
        tb = error.__traceback__.tb_next if error.__traceback__ is not None else None
        return "".join(traceback.format_exception(type(error), error, tb))
    return None


def _ensure_off_loop(name: str) -> None:
    if _loop is None:
        raise RuntimeError(f"{name}: the workset loop is not running")
    if threading.get_ident() == _thread:
        raise RuntimeError(f"{name} waits on the workset loop and cannot run on it")


class _Output:
    """sys.stdout and sys.stderr while rho runs: an `_eval`'s text goes to
    its buffer, everything else where it went before."""

    def __init__(self, inner):
        self.inner = inner

    def write(self, text):
        buffer = _output.get()
        return (buffer if buffer is not None else self.inner).write(text)

    def flush(self):
        if _output.get() is None:
            self.inner.flush()

    def __getattr__(self, name):
        return getattr(self.inner, name)


def _route_output() -> None:
    """Route sys.stdout and sys.stderr through `_Output`, again if something
    (the notebook kernel) replaced them since."""
    for name in ("stdout", "stderr"):
        stream = getattr(sys, name)
        if not isinstance(stream, _Output):
            setattr(sys, name, _Output(stream))
