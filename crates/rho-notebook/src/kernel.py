"""The notebook side of rho's Python runtime.

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
import collections
import contextvars
import heapq
import inspect
import json
import linecache
import pathlib
import re
import sys
import threading
import traceback
import types
from asyncio import events
from collections.abc import Sequence
from concurrent.futures import thread as executor_thread

CELL = contextvars.ContextVar('rho_cell', default=None)
OUTPUT_TOKEN_LIMIT = 10000
FUTURE_FLAGS = 0
for _name in __future__.all_feature_names:
    FUTURE_FLAGS |= getattr(__future__, _name).compiler_flag
# Kernel failures go to the worker's diagnostics, not a cell.
DIAGNOSTICS = sys.stderr


class Owner:
    """One cell and the work it has started."""

    def __init__(self, cell, forget):
        self.cell = cell
        self.forget = forget
        self.lock = threading.Lock()
        # The cell's own code (or open stream) until it returns.
        self.live = 1
        # Tasks and handles, for cancellation.
        self.work = set()
        self.error = None
        self.done = False

    def hold(self, work=None):
        with self.lock:
            if self.done:
                return False
            self.live += 1
            if work is not None:
                self.work.add(work)
            return True

    def release(self, work=None):
        with self.lock:
            self.work.discard(work)
            self.live -= 1
            if self.live or self.done:
                return
            self.done = True
        self.forget()
        self.cell.finished(self.error)

    def returned(self, error):
        if error is not None and self.error is None:
            self.error = error
        self.cell.returned(error)
        self.release()

    def cancel(self):
        with self.lock:
            work = list(self.work)
        if work and self.error is None:
            self.error = 'CancelledError'
        for item in work:
            # A task's scheduled step delivers the cancellation the task
            # itself receives; cancelling the step would strand the task.
            if not isinstance(getattr(getattr(item, '_callback', None), '__self__', None), asyncio.Task):
                item.cancel()


def fileno(fd):
    return fd if isinstance(fd, int) else int(fd.fileno())


def owner_of(context):
    return (CELL.get() if context is None else context.get(CELL))


class OwnedMixin:
    """A handle that holds its cell until it has run or been cancelled."""

    __slots__ = ()

    def _run(self):
        try:
            super()._run()
        finally:
            self._release()

    def cancel(self):
        if not self._cancelled:
            super().cancel()
            self._release()

    def _release(self):
        owner, self._owner = self._owner, None
        if owner is not None:
            owner.release(self)


class OwnedHandle(OwnedMixin, events.Handle):
    __slots__ = ('_owner',)


class OwnedTimerHandle(OwnedMixin, events.TimerHandle):
    __slots__ = ('_owner',)


class NotebookEventLoop(asyncio.SelectorEventLoop):
    """The stock selector loop, counting each cell's tasks, callbacks and
    descriptor watchers."""

    def __init__(self):
        # Set before the base class registers its self-pipe reader.
        self._watchers = {}
        super().__init__()

    def _owned(self, handle_type, owned_type, context, *args):
        owner = owner_of(context)
        if owner is None:
            return handle_type(*args)
        handle = owned_type(*args)
        handle._owner = owner if owner.hold(handle) else None
        return handle

    def _call_soon(self, callback, args, context):
        handle = self._owned(events.Handle, OwnedHandle, context, callback, args, self, context)
        if handle._source_traceback:
            del handle._source_traceback[-1]
        self._ready.append(handle)
        return handle

    def call_at(self, when, callback, *args, context=None):
        if when is None:
            raise TypeError("when cannot be None")
        self._check_closed()
        if self._debug:
            self._check_thread()
            self._check_callback(callback, 'call_at')
        timer = self._owned(
            events.TimerHandle, OwnedTimerHandle, context, when, callback, args, self, context)
        if timer._source_traceback:
            del timer._source_traceback[-1]
        heapq.heappush(self._scheduled, timer)
        timer._scheduled = True
        return timer

    def create_task(self, coro, **kwargs):
        task = super().create_task(coro, **kwargs)
        owner = task.get_context().get(CELL)
        if owner is not None and not task.done() and owner.hold(task):
            task.add_done_callback(lambda task: owner.release(task))
        return task

    def call_exception_handler(self, context):
        """Report a failure to the cell whose task or callback failed, and
        only to it: asyncio runs only custom handlers in the failing work's
        context, and a task can be destroyed on any thread."""
        source = context.get('future') or context.get('handle')
        get_context = getattr(source, 'get_context', None)
        source_context = get_context() if get_context else getattr(source, '_context', None)
        owner = source_context.get(CELL) if source_context is not None else None
        token = CELL.set(owner)
        try:
            return super().call_exception_handler(context)
        finally:
            CELL.reset(token)

    # A watched descriptor holds the cell that registered it until removed.

    def _watch(self, kind, fd):
        key = (kind, fileno(fd))
        self._unwatch(key)
        owner = CELL.get()
        if owner is not None and owner.hold():
            self._watchers[key] = owner

    def _unwatch(self, key):
        owner = self._watchers.pop(key, None)
        if owner is None:
            return
        if self.is_closed():
            owner.release()
        else:
            # A callback that removes its own watcher still belongs to the
            # cell until it returns, failure report included.
            self.call_soon(owner.release, context=contextvars.Context())

    def _add_reader(self, fd, callback, *args):
        self._watch('r', fd)
        return super()._add_reader(fd, callback, *args)

    def _remove_reader(self, fd):
        self._unwatch(('r', fileno(fd)))
        return super()._remove_reader(fd)

    def _add_writer(self, fd, callback, *args):
        self._watch('w', fd)
        return super()._add_writer(fd, callback, *args)

    def _remove_writer(self, fd):
        self._unwatch(('w', fileno(fd)))
        return super()._remove_writer(fd)


# A thread holds the cell it started in until it ends.

_thread_start = threading.Thread.start
_thread_bootstrap_inner = threading.Thread._bootstrap_inner


def _start(self):
    if self._target is executor_thread._worker:
        # Pool workers outlive whichever cell first used the pool; each work
        # item runs in its submitter's context.
        self._context = contextvars.Context()
    elif self._context is None and sys.flags.thread_inherit_context:
        self._context = contextvars.copy_context()
    owner = self._context.get(CELL) if self._context is not None else None
    if owner is not None and owner.hold():
        self._rho_owner = owner
    try:
        _thread_start(self)
    except BaseException:
        self._release_owner()
        raise


def _bootstrap_inner(self):
    try:
        _thread_bootstrap_inner(self)
    finally:
        self._release_owner()


def _release_owner(self):
    owner = self.__dict__.pop('_rho_owner', None)
    if owner is not None:
        owner.release()


threading.Thread.start = _start
threading.Thread._bootstrap_inner = _bootstrap_inner
threading.Thread._release_owner = _release_owner


def format_error(exc):
    """`Type: message` and the frames of cell code and libraries."""
    message = str(exc)
    lines = [f'{type(exc).__name__}: {message}' if message else type(exc).__name__]
    for frame, lineno in traceback.walk_tb(exc.__traceback__):
        filename = frame.f_code.co_filename
        if filename == __file__ or frame.f_globals.get('__name__', '').startswith('asyncio'):
            continue
        if filename.startswith('<rho-cell-'):
            filename = '<exec>'
        lines.append(f'  {filename}:{lineno} in {frame.f_code.co_name}')
    return '\n'.join(lines)


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
    """A cell whose source arrives in pieces and runs one admitted top-level
    statement at a time."""

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

    def run(self):
        self.loop = NotebookEventLoop()
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
        owner = Owner(cell, lambda: self.cells.pop(cell.id, None))
        self.cells[cell.id] = owner
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
        """Run compiled code in the cell's context; `done(error)` once it has
        returned. Awaiting code starts as an eager task, so code before its
        first suspension runs before later inputs."""
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

        def finished(task):
            if task.cancelled():
                done('CancelledError')
            elif task.exception() is not None:
                done(format_error(task.exception()))
            else:
                done(None)

        task = self.loop.create_task(result, context=context, eager_start=True)
        if task.done():
            finished(task)
        else:
            task.add_done_callback(finished, context=self.context)

    def on_execute(self, cell, source):
        owner = self.owner(cell)
        cell.started()
        filename = f'<rho-cell-{cell.id}>'
        try:
            code = self.compile(source, filename)
        except BaseException as exc:
            owner.returned(format_error(exc))
            return
        self.run_code(owner, code, owner.returned)

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
        self.advance(stream)

    def on_permit(self, cell, end):
        stream = self.streams.get(cell)
        if stream is None or stream.stopped or stream.pending is None or stream.pending[0] != end:
            return
        code = stream.pending[1]
        stream.pending = None
        stream.running = end
        stream.owner.hold()
        self.run_code(stream.owner, code, lambda error: self.settled(stream, end, error))

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
        owner = self.cells.get(cell)
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
        elif stream.eof and stream.pos >= len(stream.text):
            self.close(stream, None)

    def settled(self, stream, end, error):
        stream.owner.cell.unit_settled(end, error)
        stream.running = None
        if error is not None or stream.stopped:
            self.close(stream, error)
        else:
            self.advance(stream)
        stream.owner.release()

    def close(self, stream, error):
        if self.streams.get(stream.owner.cell.id) is stream:
            del self.streams[stream.owner.cell.id]
            stream.owner.returned(error)


# The Python-facing API.

def current_cell(purpose):
    owner = CELL.get()
    if owner is None:
        raise RuntimeError(f'{purpose} only while a cell runs')
    return owner.cell


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
    if not is_int(seconds) or not 1 <= seconds <= 3600:
        raise ValueError('seconds must be an integer from 1 through 3600')
    owner = CELL.get()
    if owner is not None:
        owner.cell.max_wait(seconds)


def suppress_tool_wakeups():
    owner = CELL.get()
    if owner is not None:
        owner.cell.suppress_tool_wakeups()


HISTORY_FIELDS = (
    'kind', 'role', 'sender', 'text', 'content', 'name', 'call_id', 'summary', 'images',
    'provider', 'status', 'phase', 'tool_type', 'started_at', 'finished_at', 'at',
    'retain_from', 'call_ids', 'response_id', 'metadata',
)
HistoryContent = collections.namedtuple(
    'HistoryContent', ['kind', 'text', 'media_type', 'data'], defaults=(None, None, None),
    module='__main__')
HistoryImage = collections.namedtuple(
    'HistoryImage', ['media_type', 'data', 'detail'], defaults=(None,), module='__main__')
HistoryProviderData = collections.namedtuple(
    'HistoryProviderData', ['tag', 'data'], module='__main__')
HistoryProviderData.__repr__ = (
    lambda self: f'HistoryProviderData(tag={self.tag!r}, data=<{len(self.data)} bytes>)')
HistoryItem = collections.namedtuple(
    'HistoryItem', HISTORY_FIELDS,
    defaults=tuple(() if field in ('content', 'summary', 'images', 'call_ids') else None
                   for field in HISTORY_FIELDS[1:]),
    module='__main__')
HistoryItem.arguments = property(
    lambda self: self.text if self.kind == 'tool_call' else None,
    doc="A tool call's arguments.")


def freeze(value):
    if isinstance(value, dict):
        return types.MappingProxyType({key: freeze(item) for key, item in value.items()})
    if isinstance(value, list):
        return tuple(freeze(item) for item in value)
    return value


def history_item(raw):
    fields = dict(zip(HISTORY_FIELDS, raw))
    fields['content'] = tuple(HistoryContent(*part) for part in fields['content'])
    fields['images'] = tuple(HistoryImage(*image) for image in fields['images'])
    if fields['provider'] is not None:
        fields['provider'] = HistoryProviderData(*fields['provider'])
    if fields['metadata'] is not None:
        fields['metadata'] = freeze(json.loads(fields['metadata']))
    return HistoryItem(**fields)


class Transcript(Sequence):
    """The running cell's view of the conversation."""

    def _cell(self):
        return current_cell('transcript is available')

    def __len__(self):
        return self._cell().history_len()

    def __getitem__(self, index):
        cell = self._cell()
        length = cell.history_len()
        if isinstance(index, slice):
            return tuple(history_item(cell.history_get(at)) for at in range(*index.indices(length)))
        if not is_int(index):
            raise TypeError('transcript indices must be integers or slices')
        if index < 0:
            index += length
        if not 0 <= index < length:
            raise IndexError('transcript index out of range')
        return history_item(cell.history_get(index))

    def __iter__(self):
        """A snapshot: items appended while iterating are not included."""
        return iter(self[:])

    def __repr__(self):
        return f'transcript({len(self)} items)'


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
        'transcript': Transcript(),
        'asyncio': asyncio,
        'Path': pathlib.Path,
        'pathlib': pathlib,
        **modules,
    }


sys.stdout = sys.__stdout__ = Output()
sys.stderr = sys.__stderr__ = Output()
