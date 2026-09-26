"""The notebook side of rho-notebook2's Python runtime.

Each notebook runs `Notebook.run` on its own thread and event loop, with its
own globals. A cell is an `Owner` stored in the `CELL` context variable while
its code runs. Asyncio copies the context into every task and callback it
schedules and threads inherit it, so the owner counts everything the cell
started: the cell has finished when its code has returned and that count
reaches zero. The same variable says where output goes.
"""
import __future__
import asyncio
import ast
import builtins
import contextvars
import heapq
import inspect
import linecache
import pathlib
import re
import sys
import threading
import traceback
from asyncio import events
from concurrent.futures import thread as executor_thread

CELL = contextvars.ContextVar('rho_cell', default=None)
OUTPUT_TOKEN_LIMIT = 10000
FUTURE_FLAGS = 0
for _name in __future__.all_feature_names:
    FUTURE_FLAGS |= getattr(__future__, _name).compiler_flag
# Kernel failures go to the worker's diagnostics, not a cell.
DIAGNOSTICS = sys.stderr


class Owner:
    """One task's output, commands and completion; children copy its context only."""

    def __init__(self, cell, notebook):
        self.cell = cell
        self.notebook = notebook
        self.commands = set()
        self.task = None
        self.done = False

    def __await__(self):
        return self.task.__await__()

    def failed(self, exc):
        self.cell.pending_failure()
        self.done = True
        task = self.task
        def report():
            if task._log_traceback:
                self.cell.finished(format_error(exc, self.cell.id), False)
                task.exception()
            else:
                self.cell.claimed()
            self.notebook.tasks.pop(self.cell.id, None)
        self.notebook.loop.call_later(20, report, context=self.notebook.context)

    def command(self, future):
        self.commands.add(future)
        future.add_done_callback(self.commands.discard)

    async def finish(self, code):
        try:
            value = await code
        except asyncio.CancelledError:
            self.cell.cancel_commands()
            self.cell.finished(None, True)
            self.notebook.tasks.pop(self.cell.id, None)
            self.done = True
            raise
        except BaseException as exc:
            self.failed(exc)
            raise
        # A command belongs to this task even if nobody explicitly awaited it.
        while self.commands:
            await asyncio.gather(*(asyncio.shield(f) for f in tuple(self.commands)))
        self.cell.finished(None, False)
        self.notebook.tasks.pop(self.cell.id, None)
        self.done = True
        return value

    def cancel(self):
        self.cell.cancel_commands()
        if self.task is not None:
            self.task.cancel()


class NotebookEventLoop(asyncio.SelectorEventLoop):
    def __init__(self, notebook):
        super().__init__()
        self.notebook = notebook

    def run_in_executor(self, executor, func, *args):
        context = contextvars.copy_context()
        return super().run_in_executor(executor, context.run, func, *args)

    def create_task(self, coro, **kwargs):
        creator = owner_of(kwargs.get('context'))
        if creator is None:
            return super().create_task(coro, **kwargs)
        name = getattr(coro, '__name__', type(coro).__name__)
        cell = self.notebook.driver.new_task(creator.cell.id, name)
        owner = Owner(cell, self.notebook)
        context = kwargs.pop('context', None) or contextvars.copy_context()
        context.run(CELL.set, owner)
        task = super().create_task(owner.finish(coro), context=context, **kwargs)
        owner.task = task
        self.notebook.tasks[cell.id] = owner
        def cancelled(task):
            if task.cancelled() and not owner.done:
                cell.cancel_commands()
                cell.finished(None, True)
                owner.done = True
                self.notebook.tasks.pop(cell.id, None)
        task.add_done_callback(cancelled, context=self.notebook.context)
        return task


def owner_of(context):
    return CELL.get() if context is None else context.get(CELL)


# Thread output inherits the starting task's context, without holding it open.
_thread_start = threading.Thread.start

def _start(self):
    if self._target is executor_thread._worker:
        self._context = contextvars.Context()
    elif self._context is None and CELL.get() is not None:
        self._context = contextvars.copy_context()
    return _thread_start(self)

threading.Thread.start = _start

def format_error(exc, task_id=None):
    """`Type: message` and the frames of task code and libraries."""
    message = str(exc)
    lines = [f'{type(exc).__name__}: {message}' if message else type(exc).__name__]
    for frame, lineno in traceback.walk_tb(exc.__traceback__):
        filename = frame.f_code.co_filename
        if filename == __file__ or frame.f_globals.get('__name__', '').startswith('asyncio'):
            continue
        if filename.startswith('<rho-cell-'):
            from_id = int(filename[len('<rho-cell-'):-1])
            filename = f'<task {session_id(task_id or from_id)}>'
        lines.append(f'  {filename}:{lineno} in {frame.f_code.co_name}')
    return '\n'.join(lines)


def session_id(internal_id):
    x = internal_id % 9000
    left, right = divmod(x, 100)
    left = (left + right * right + 17 * right + 43) % 90
    right = (right + left * left + 29 * left + 71) % 100
    left = (left + right * right + 53 * right + 19) % 90
    right = (right + left * left + 11 * left + 37) % 100
    return 1000 + 100 * left + right


class Task:
    """A task named in a report; awaiting propagates its original exception."""
    @staticmethod
    def from_session_id(label):
        notebook = NOTEBOOK
        matches = [owner for id, owner in notebook.tasks.items()
                   if session_id(id) == label]
        if len(matches) != 1:
            raise RuntimeError(f'No unique live task has session ID {label}')
        return matches[0]

    def __await__(self):
        return self.task.__await__()


# Statements a later line can extend.
BLOCKS = (
    ast.If, ast.For, ast.AsyncFor, ast.While, ast.With, ast.AsyncWith, ast.Try, ast.TryStar,
    ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Match,
)
CONTINUATION = re.compile(r'(else|elif|except|finally)\b')


def starts_statement(line):
    """Whether `line` can only begin a new top-level statement."""
    return bool(line) and line[0] not in ' \t\r\n\f#' and not CONTINUATION.match(line)


class Stream:
    """A cell whose source arrives in pieces and runs one top-level statement
    at a time: each admitted while the source is still arriving, and the rest
    freely once it has all arrived."""

    def __init__(self, owner, filename):
        self.owner = owner
        self.filename = filename
        self.text = ''
        self.pos = 0        # characters compiled so far
        self.end = 0        # the same, in UTF-8 bytes
        self.lines = 0      # newlines compiled so far
        self.eof = False
        self.pending = None  # (end, code) waiting for its permit
        self.running = None  # the end of the unit now running
        self.stopped = False

    def next_unit(self, compile_unit):
        """The next complete statement as `(end, code)`, or None until more
        source arrives. A simple statement is complete at the end of its
        line; a block once a line that can only start another statement
        follows it, or at end of input."""
        rest = self.text[self.pos:]
        lines = rest.split('\n')
        offset = 0
        for index, line in enumerate(lines[:-1]):
            offset += len(line) + 1
            following = lines[index + 1]
            if starts_statement(following):
                unit = self.try_unit(rest[:offset], compile_unit, final=False, blocks=True)
            elif not following.strip() or following.lstrip().startswith('#'):
                unit = self.try_unit(rest[:offset], compile_unit, final=False, blocks=False)
            else:
                continue
            if unit is not None:
                return unit
            if self.text[self.pos:] != rest:
                # Only blank lines and comments: look past them.
                return self.next_unit(compile_unit)
        if self.eof and rest:
            return self.try_unit(rest, compile_unit, final=True, blocks=True)
        return None

    def try_unit(self, chunk, compile_unit, final, blocks):
        """Take `chunk` as the next unit if it parses. Unless `blocks`, only a
        chunk ending in a simple statement is complete."""
        padded = '\n' * self.lines + chunk
        try:
            tree = ast.parse(padded, self.filename)
        except SyntaxError:
            if final:
                raise
            return None
        if not blocks and tree.body and isinstance(tree.body[-1], BLOCKS):
            return None
        self.pos += len(chunk)
        self.end += len(chunk.encode())
        self.lines += chunk.count('\n')
        if not tree.body:
            return None
        return self.end, compile_unit(tree, self.filename)


class Notebook:
    """One notebook: its globals, event loop and cells."""

    def __init__(self, driver, exports):
        self.driver = driver
        self.flags = 0
        self.cells = {}
        self.streams = {}
        self.context = contextvars.copy_context()
        self.namespace = namespace(exports)
        self.loop = None
        self.tasks = {}
        global NOTEBOOK
        NOTEBOOK = self

    def run(self):
        self.loop = NotebookEventLoop(self)
        self.loop.add_reader(self.driver.fd, self.drain)
        try:
            self.loop.run_forever()
        finally:
            self.loop.remove_reader(self.driver.fd)

    def drain(self):
        for message in self.driver.drain():
            try:
                getattr(self, 'on_' + message[0])(*message[1:])
            except BaseException:
                traceback.print_exc(file=DIAGNOSTICS)

    def owner(self, cell):
        owner = Owner(cell, self)
        self.cells[cell.id] = owner
        self.tasks[cell.id] = owner
        return owner

    def compile(self, source, filename):
        """Compile cell code, keeping `from __future__` imports for later
        cells like an interactive interpreter does."""
        text = source if isinstance(source, str) else None
        if text is not None:
            linecache.cache[filename] = (len(text), None, text.splitlines(True), filename)
        code = compile(source, filename, 'exec',
                       self.flags | ast.PyCF_ALLOW_TOP_LEVEL_AWAIT, dont_inherit=True)
        self.flags |= code.co_flags & FUTURE_FLAGS
        return code

    def run_code(self, owner, code, done):
        context = self.context.copy()
        context.run(CELL.set, owner)
        try:
            result = context.run(eval, code, self.namespace)
        except BaseException as exc:
            done(format_error(exc))
            return
        if not code.co_flags & inspect.CO_COROUTINE:
            done(None)
            return

        async def run():
            return await result

        # This is the exec's own code, not a created child task.
        task = asyncio.tasks.Task(run(), loop=self.loop, context=context, eager_start=True)
        owner.task = task
        def finished(task):
            if task.cancelled():
                owner.cell.cancel_commands()
                owner.cell.finished(None, True)
                owner.done = True
            elif task._exception is not None:
                owner.cell.returned(None)
                owner.failed(task._exception)
            elif not owner.done:
                done(None)
        if task.done():
            finished(task)
        else:
            task.add_done_callback(finished, context=self.context)

    def finish_owner(self, owner, error):
        if owner.done:
            return
        if error is not None:
            owner.done = True
            owner.cell.returned(error)
            owner.cell.finished(None, False)
            self.tasks.pop(owner.cell.id, None)
            self.cells.pop(owner.cell.id, None)
            return
        owner.cell.returned(None)
        async def wait():
            while owner.commands:
                await asyncio.gather(*(asyncio.shield(f) for f in tuple(owner.commands)))
            owner.done = True
            owner.cell.finished(None, False)
            self.tasks.pop(owner.cell.id, None)
            self.cells.pop(owner.cell.id, None)
        # An implicit wait must not create another reported task.
        asyncio.tasks.Task(wait(), loop=self.loop, context=self.context)

    def on_execute(self, cell, source):
        owner = self.owner(cell)
        cell.started()
        filename = f'<rho-cell-{cell.id}>'
        try:
            code = self.compile(source, filename)
        except BaseException as exc:
            self.finish_owner(owner, format_error(exc))
            return
        self.run_code(owner, code, lambda error: self.finish_owner(owner, error))

    def on_begin(self, cell):
        owner = self.owner(cell)
        cell.started()
        self.streams[cell.id] = Stream(owner, f'<rho-cell-{cell.id}>')

    def on_feed(self, cell, source, eof):
        stream = self.streams.get(cell)
        if stream is None or stream.stopped:
            return
        stream.text += source
        stream.eof = stream.eof or eof
        linecache.cache[stream.filename] = (
            len(stream.text), None, stream.text.splitlines(True), stream.filename)
        if stream.eof and stream.pending is not None:
            self.run_unit(stream)
        else:
            self.advance(stream)

    def on_permit(self, cell, end):
        stream = self.streams.get(cell)
        if stream is None or stream.stopped or stream.pending is None or stream.pending[0] != end:
            return
        self.run_unit(stream)

    def on_stop(self, cell):
        stream = self.streams.get(cell)
        if stream is None:
            return
        stream.stopped = True
        stream.pending = None
        if stream.running is None:
            self.close(stream, None)

    def on_cancel(self, cell):
        self.on_stop(cell)
        owner = self.tasks.get(cell)
        if owner is not None:
            owner.cancel()

    def on_shutdown(self):
        self.loop.stop()

    def advance(self, stream):
        """Offer the next compiled unit, or close the stream at its end."""
        if stream.stopped or stream.pending is not None or stream.running is not None:
            return
        try:
            unit = stream.next_unit(self.compile)
        except SyntaxError as exc:
            self.close(stream, format_error(exc))
            return
        if unit is not None:
            stream.pending = unit
            stream.owner.cell.unit_ready(unit[0])
            if stream.eof:
                self.run_unit(stream)
        elif stream.eof and stream.pos >= len(stream.text):
            self.close(stream, None)

    def run_unit(self, stream):
        end, code = stream.pending
        stream.pending = None
        stream.running = end
        self.run_code(stream.owner, code, lambda error: self.settled(stream, end, error))

    def settled(self, stream, end, error):
        stream.owner.cell.unit_settled(end, error)
        stream.running = None
        if error is not None or stream.stopped:
            self.close(stream, error)
        else:
            self.advance(stream)

    def close(self, stream, error):
        if self.streams.get(stream.owner.cell.id) is stream:
            del self.streams[stream.owner.cell.id]
            self.finish_owner(stream.owner, error)


# The Python-facing API.

def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def budget(max_tokens):
    if max_tokens is None:
        return 2000
    if not is_int(max_tokens) or max_tokens < 1:
        raise ValueError('max_tokens must be a positive integer')
    return min(max_tokens, OUTPUT_TOKEN_LIMIT)


def render(text, max_tokens):
    limit = max_tokens * 4
    return text if len(text) <= limit else text[:limit] + '\n[truncated]'


def emit(text, max_tokens, important):
    owner = CELL.get()
    if owner is not None:
        owner.cell.text(text, important)


class Output:
    """`sys.stdout` and `sys.stderr`: text goes to the running code's cell."""

    encoding = 'utf-8'
    errors = 'replace'
    closed = False

    def write(self, text):
        if not isinstance(text, str):
            raise TypeError('write() requires a string')
        if text:
            emit(render(text, OUTPUT_TOKEN_LIMIT), OUTPUT_TOKEN_LIMIT, False)
        return len(text)

    def flush(self):
        pass

    def isatty(self):
        return False

    def writable(self):
        return True


def print(*values, sep=' ', end='\n', file=None, flush=False, max_tokens=None):
    if file is not None:
        return builtins.print(*values, sep=sep, end=end, file=file, flush=flush)
    tokens = budget(max_tokens)
    sep = ' ' if sep is None else sep
    end = '\n' if end is None else end
    emit(render(sep.join(map(str, values)) + end, tokens), tokens, False)


def notify(value, *, max_tokens=None):
    tokens = budget(max_tokens)
    emit(render(str(value), tokens) + '\n', tokens, True)


def set_max_wait(seconds):
    if not is_int(seconds) or seconds < 1:
        raise ValueError('seconds must be a positive integer')
    owner = CELL.get()
    if owner is not None:
        owner.cell.max_wait(seconds)


def suppress_tool_wakeups():
    owner = CELL.get()
    if owner is not None:
        owner.cell.suppress_tool_wakeups()


def namespace(exports):
    """A notebook's globals, with the objects its host exports. Those are
    also importable, from the notebook's code only."""
    modules = dict(exports)

    def notebook_import(name, globals=None, locals=None, fromlist=(), level=0):
        if level == 0 and name in modules:
            return modules[name]
        return builtins.__import__(name, globals, locals, fromlist, level)

    return {
        '__name__': '__main__',
        '__builtins__': dict(vars(builtins), __import__=notebook_import),
        'print': print,
        'notify': notify,
        'set_max_wait': set_max_wait,
        'suppress_tool_wakeups': suppress_tool_wakeups,
        'asyncio': asyncio,
        'Task': Task,
        'Path': pathlib.Path,
        'pathlib': pathlib,
        **modules,
    }


sys.stdout = sys.__stdout__ = Output()
sys.stderr = sys.__stderr__ = Output()
