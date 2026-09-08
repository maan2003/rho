"""Host-driven notebook scheduler. No Python objects cross the host boundary."""
import ast
import asyncio
import contextvars
import json
import pathlib
import sys
import types

_cell = contextvars.ContextVar('rho_cell')
_requests = {}
_cells = {}
_sequence = 0


def _format_error(exc):
    lines = [f'{type(exc).__name__}: {exc}']
    tb = exc.__traceback__
    while tb is not None:
        code = tb.tb_frame.f_code
        if code.co_filename != '<rho-runtime>':
            lines.append(f'  {code.co_filename}:{tb.tb_lineno} in {code.co_name}')
        tb = tb.tb_next
    return '\n'.join(lines)

def _trace(frame, event, arg):
    # Interrupt user bytecode, never asyncio's task/selector bookkeeping.
    if frame.f_code.co_filename.startswith('<rho-cell-') and _cancel_requested(_cell.get(0)):
        raise asyncio.CancelledError()
    return _trace


def _task_done(task, cell):
    state = _cells[cell]
    state['tasks'].discard(task)
    if not task.cancelled() and task._exception is not None:
        state['errors'].append(task)


def _task_factory(loop, coroutine, context=None, **kwargs):
    context = contextvars.copy_context() if context is None else context
    cell = context.get(_cell)
    if cell in _cells and sum(len(state['tasks']) for state in _cells.values()) >= 1024:
        coroutine.close()
        raise RuntimeError('Notebook task limit (1024) reached')
    task = asyncio.Task(coroutine, loop=loop, context=context, **kwargs)
    if cell in _cells:
        _cells[cell]['tasks'].add(task)
        task.add_done_callback(lambda done: _task_done(done, cell), context=contextvars.Context())
    return task


class _NotebookLoop(asyncio.SelectorEventLoop):
    # The stdlib owns scheduling, futures, timers and I/O. This hook only
    # determines when the work attributed to a notebook cell has finished.
    def _run_once(self):
        super()._run_once()
        active = {handle._context.get(_cell) for handle in [*self._ready, *self._scheduled]
                  if not handle.cancelled()}
        for key in self._selector.get_map().values():
            active.update(handle._context.get(_cell) for handle in key.data
                          if handle is not None and not handle.cancelled())
        for cell, state in list(_cells.items()):
            if state['tasks'] or cell in active:
                continue
            root = state['root']
            if not root.done():
                continue
            error = state['error']
            if root.cancelled():
                error = error or 'CancelledError'
            elif root.exception() is not None:
                error = error or _format_error(root.exception())
            errors = [error] if error else []
            for task in state['errors']:
                if task is not root and task._log_traceback:
                    errors.append(_format_error(task.exception()))
            del _cells[cell]
            _send('finished', cell=cell, error='\n'.join(errors) or None)


def _loop_error(loop, context):
    handle = context.get('handle')
    cell = handle._context.get(_cell) if handle is not None else _cell.get(None)
    if cell in _cells:
        exc = context.get('exception')
        _cells[cell]['error'] = _format_error(exc) if exc else context['message']
    else:
        loop.default_exception_handler(context)


_loop = _NotebookLoop()
_loop.set_task_factory(_task_factory)
_loop.set_exception_handler(_loop_error)


def _send(kind, **fields):
    _emit(json.dumps(dict(kind=kind, **fields)))


def _budget(value):
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise ValueError('max_tokens must be a positive integer')
    return min(value, 10000)


def _request(name, arguments):
    global _sequence
    if len(_requests) >= 1024:
        raise RuntimeError('Too many pending host requests')
    _sequence += 1
    result = _loop.create_future()
    result.id = _sequence
    _requests[result.id] = result
    try:
        _send('call', cell=_cell.get(), request=result.id, name=name, arguments=arguments)
    except BaseException:
        del _requests[result.id]
        raise
    return result

class Command:
    def __init__(self, result):
        self._result = result
        self.id = result.id

    def __await__(self):
        return asyncio.shield(self._result).__await__()

    def cancel(self):
        return _request('cancel_command', {'id': self.id})

    def __repr__(self):
        return f'<command {self.id}>'


def command(cmd, *, workdir=None, max_tokens=2000):
    if not isinstance(cmd, str):
        raise TypeError('command must be a string')
    return Command(_request('command', dict(cmd=cmd, workdir=workdir, max_tokens=_budget(max_tokens))))


def write_stdin(handle, chars='', *, max_tokens=2000):
    return _request('write_stdin', dict(id=handle.id, chars=chars, max_tokens=_budget(max_tokens)))


def display(value, *, max_tokens=2000):
    if isinstance(value, Command):
        return _request('display', dict(id=value.id, max_tokens=_budget(max_tokens)))
    text(value, max_tokens=max_tokens)


def _render(value, budget):
    value = str(value)
    limit = _budget(budget) * 4
    if len(value) > limit:
        return value[:limit] + '\n[truncated]'
    return value


def text(value, *, max_tokens=2000):
    max_tokens = _budget(max_tokens)
    _send('text', cell=_cell.get(), text=_render(value, max_tokens) + '\n', max_tokens=max_tokens, important=False)


def notify(value, *, max_tokens=2000):
    max_tokens = _budget(max_tokens)
    _send('text', cell=_cell.get(), text=_render(value, max_tokens) + '\n', max_tokens=max_tokens, important=True)


def set_patience(seconds=300):
    if isinstance(seconds, bool) or not isinstance(seconds, int) or not 1 <= seconds <= 3600:
        raise ValueError('seconds must be an integer from 1 through 3600')
    _send('patience', cell=_cell.get(), seconds=seconds)

def image(reference):
    return _request('image', reference)


class Tools:
    def __getattr__(self, name):
        def call(arguments=None, **kwargs):
            if arguments is not None and kwargs:
                raise TypeError('Pass either an argument or keyword arguments')
            return _request(name, kwargs if arguments is None else arguments)
        return call

class _Output:
    encoding = 'utf-8'
    errors = 'replace'
    closed = False

    def write(self, value):
        if not isinstance(value, str):
            raise TypeError('write() requires a string')
        cell = _cell.get(None)
        if value and cell is not None:
            _send('text', cell=cell, text=_render(value, 10000), max_tokens=10000, important=False)
        return len(value)

    def flush(self):
        pass

    def isatty(self):
        return False

    def writable(self):
        return True


# No daemon descriptors: ordinary print/library diagnostics are attributed
# through the same context as explicit text, including partial writes.
sys.stdout = sys.__stdout__ = _Output()
sys.stderr = sys.__stderr__ = _Output()

_namespace = dict(__name__='__main__', command=command, write_stdin=write_stdin,
                  display=display, text=text, notify=notify, set_patience=set_patience,
                  tools=Tools(), image=image, asyncio=asyncio, pathlib=pathlib, Path=pathlib.Path)

def _execute(cell, source):
    if len(_cells) >= 128 or cell in _cells:
        _send('finished', cell=cell, error='Notebook live cell limit reached or duplicate cell')
        return
    _cells[cell] = dict(root=None, tasks=set(), errors=[], error=None, cancelled=False)
    context = contextvars.copy_context()
    context.run(_cell.set, cell)
    async def evaluate():
        code = compile(source, f'<rho-cell-{cell}>', 'exec', flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
        result = eval(code, _namespace)
        if isinstance(result, types.CoroutineType):
            await result
    try:
        _cells[cell]['root'] = _loop.create_task(evaluate(), context=context)
    except BaseException as exc:
        del _cells[cell]
        _send('finished', cell=cell, error=_format_error(exc))


def _receive_ready():
    for message in json.loads(_receive()):
        kind = message['kind']
        if kind == 'shutdown':
            _loop.stop()
            return
        if kind == 'execute':
            _execute(message['cell'], message['source'])
        elif kind == 'resolve':
            future = _requests.pop(message['request'], None)
            if future is not None and not future.done():
                error = message.get('error')
                if error:
                    future.set_exception(RuntimeError(error))
                    # Rust already reports host failures; don't also log an
                    # unawaited-future warning. Await still raises the exception.
                    future.exception()
                else:
                    future.set_result(message.get('value'))
        elif kind == 'cancel':
            cell = message['cell']
            _cancel_requested(cell)
            state = _cells.get(cell)
            if state is None or state['cancelled']:
                continue
            state['cancelled'] = True
            state['error'] = 'CancelledError'
            for task in list(state['tasks']):
                task.cancel()
            for handle in [*_loop._ready, *_loop._scheduled]:
                callback = getattr(handle._callback, 'func', handle._callback)
                module = getattr(callback, '__module__', '') or ''
                if (handle._context.get(_cell) == cell
                        and not module.startswith(('asyncio', '_asyncio'))
                        and not isinstance(getattr(callback, '__self__', None), asyncio.Task)):
                    handle.cancel()
            for key in list(_loop._selector.get_map().values()):
                reader, writer = key.data
                if reader is not None and reader._context.get(_cell) == cell:
                    _loop.remove_reader(key.fd)
                if writer is not None and writer._context.get(_cell) == cell:
                    _loop.remove_writer(key.fd)


def _run():
    _loop.add_reader(_inbox_fd, _receive_ready)
    sys.settrace(_trace)
    try:
        _loop.run_forever()
    finally:
        sys.settrace(None)
        _loop.close()
