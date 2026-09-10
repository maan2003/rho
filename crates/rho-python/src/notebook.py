"""Host-driven notebook scheduler. No Python objects cross the host boundary."""
import ast
import asyncio
import _asyncio
import contextvars
import json
import pathlib
import sys
import types
import inspect
import threading
import concurrent.futures

sys.path.insert(0, _site_packages)
sys.path.insert(0, '')

_cell = contextvars.ContextVar('rho_cell')
_requests = {}
_cells = {}
_streams = {}
_sequence = 0
_notebook_thread = threading.current_thread()
_SYNC_TIMEOUT = 120
_HEARTBEAT_INTERVAL = 10
_task_dispatch = False


def _heartbeat():
    global _heartbeat_handle
    _sync_watchdog(_SYNC_TIMEOUT)
    _heartbeat_handle = _loop.call_later(
        _HEARTBEAT_INTERVAL, _heartbeat, context=contextvars.Context())


def _interruptible(frame):
    # Only cross an explicit user-code dispatch boundary. In particular, a
    # user's __repr__ called by runtime/asyncio bookkeeping is not safe to stop.
    candidate = frame
    task = asyncio.current_task()
    coroutine = task.get_coro() if task is not None else None
    task_frame = getattr(coroutine, 'cr_frame', getattr(coroutine, 'gi_frame', None))
    while frame is not None:
        code = frame.f_code
        if code is _evaluate.__code__:
            return frame is not candidate
        if code is _handle_run.__code__:
            return frame is not candidate and not _task_dispatch
        module = frame.f_globals.get('__name__', '')
        if code.co_filename == '<rho-runtime>' or module.split('.')[0] in ('asyncio', '_asyncio'):
            return False
        if frame is task_frame:
            return True
        frame = frame.f_back
    return False


def _format_error(exc):
    lines = [f'{type(exc).__name__}: {exc}']
    tb = exc.__traceback__
    while tb is not None:
        code = tb.tb_frame.f_code
        if code.co_filename != '<rho-runtime>':
            filename = '<exec>' if code.co_filename.startswith('<rho-cell-') else code.co_filename
            lines.append(f'  {filename}:{tb.tb_lineno} in {code.co_name}')
        tb = tb.tb_next
    return '\n'.join(lines)

def _trace(frame, event, arg):
    filename = frame.f_code.co_filename
    # These frames cannot be interrupted. Returning None on entry skips their
    # line events; calls into user/library code still enter this global hook.
    if event == 'call':
        module = frame.f_globals.get('__name__', '')
        if filename == '<rho-runtime>' or module.split('.')[0] in ('asyncio', '_asyncio'):
            return None
    if (filename.startswith('<rho-cell-')
            and (_cells.get(_cell.get(None), {}).get('cancelled', False)
                 or _cancel_requested(_cell.get(0), False))):
        raise asyncio.CancelledError()
    if (_sync_watchdog() and _cell.get(None) in _cells and _interruptible(frame)):
        # Consume this deadline before unwinding, so another ready cell cannot
        # inherit it. Cancellation of workers remains independent of this timer.
        _sync_watchdog(_SYNC_TIMEOUT)
        raise TimeoutError('Cell interrupted after blocking the event loop for 2 minutes')
    return _trace


_handle_run = asyncio.Handle._run


def _run_handle(handle):
    global _task_dispatch
    if handle._loop is not _loop:
        return _handle_run(handle)
    previous = _task_dispatch
    _sync_watchdog(_SYNC_TIMEOUT)
    # Native task bookkeeping has no Python stack frames. Only its running
    # coroutine's exact frame authorizes interruption, not Handle dispatch.
    _task_dispatch = isinstance(
        handle._callback, (_asyncio.TaskStepMethWrapper, _asyncio.TaskWakeupMethWrapper))
    try:
        return _handle_run(handle)
    finally:
        _task_dispatch = previous
        _sync_watchdog(_SYNC_TIMEOUT)
        # A trace exception may disable tracing. Re-arm at a protected dispatch
        # boundary, including between callbacks in the same event-loop tick.
        sys.settrace(_trace)


asyncio.Handle._run = _run_handle


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
    def run_in_executor(self, executor, func, *args):
        self._check_closed()
        if executor is None:
            self._check_default_executor()
            if self._default_executor is None:
                self._default_executor = concurrent.futures.ThreadPoolExecutor(
                    thread_name_prefix='asyncio')
            executor = self._default_executor
        cell = _cell.get(None)
        state = _cells.get(cell)
        if state is not None:
            state['workers'].add(worker := object())
        try:
            # Pool threads belong to the executor, not the cell that creates them.
            if isinstance(executor, concurrent.futures.ThreadPoolExecutor):
                future = contextvars.Context().run(
                    executor.submit, _executor_call, cell, func, args)
            else:
                future = contextvars.Context().run(executor.submit, func, *args)
        except BaseException:
            if state is not None:
                state['workers'].remove(worker)
            raise
        if state is not None:
            future.add_done_callback(
                lambda _: self.call_soon_threadsafe(_worker_done, cell, worker, context=contextvars.Context()))
        return asyncio.wrap_future(future, loop=self)

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
            if state['tasks'] or state['workers'] or cell in active:
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


def _executor_call(cell, func, args):
    token = _cell.set(cell)
    try:
        return func(*args)
    finally:
        _cell.reset(token)


def _worker_done(cell, worker):
    _cells[cell]['workers'].remove(worker)


_thread_start = threading.Thread.start
_thread_bootstrap = threading.Thread._bootstrap_inner


def _start_thread(thread, *args, **kwargs):
    if not thread._initialized or thread._started.is_set():
        return _thread_start(thread, *args, **kwargs)
    cell = _cell.get(None)
    state = _cells.get(cell)
    if state is not None:
        state['workers'].add(thread)
        thread._rho_cell = cell
        # Preserve normal explicit Thread(context=...) behavior.
        if thread._context is None:
            thread._context = contextvars.copy_context()
    try:
        return _thread_start(thread, *args, **kwargs)
    except BaseException:
        if state is not None:
            state['workers'].remove(thread)
            del thread._rho_cell
        raise


def _bootstrap_thread(thread):
    cell = getattr(thread, '_rho_cell', None)
    token = _cell.set(cell)
    try:
        return _thread_bootstrap(thread)
    finally:
        _cell.reset(token)
        if cell is not None:
            _loop.call_soon_threadsafe(_worker_done, cell, thread, context=contextvars.Context())


threading.Thread.start = _start_thread
threading.Thread._bootstrap_inner = _bootstrap_thread


def _send(kind, **fields):
    _emit(json.dumps(dict(kind=kind, **fields)))


def _budget(value):
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise ValueError('max_tokens must be a positive integer')
    return min(value, 10000)


def _request(name, arguments):
    if threading.current_thread() is not _notebook_thread:
        raise RuntimeError('Call host functions from the notebook event loop, not a worker thread')
    global _sequence
    if len(_requests) >= 1024:
        raise RuntimeError('Too many pending host requests')
    result = _loop.create_future()
    result.id = _sequence
    _sequence += 1
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
    if inspect.isfunction(value):
        docs = f'{value.__module__}.{value.__name__}{inspect.signature(value)}\n\n{inspect.getdoc(value) or ""}'
    else:
        text(value, max_tokens=max_tokens)
        return
    text(docs, max_tokens=max_tokens)
    return docs


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


def set_checkin(after_seconds=300, *, wake_on_tools=True):
    if isinstance(after_seconds, bool) or not isinstance(after_seconds, int) or not 1 <= after_seconds <= 3600:
        raise ValueError('after_seconds must be an integer from 1 through 3600')
    if not isinstance(wake_on_tools, bool):
        raise TypeError('wake_on_tools must be a bool')
    _send('checkin', cell=_cell.get(), seconds=after_seconds, wake_on_tools=wake_on_tools)

def image(reference):
    return _request('image', reference)


def _host_function(name):
    def call(arguments=None, **kwargs):
        if arguments is not None and kwargs:
            raise TypeError('Pass either an argument or keyword arguments')
        return _request(name, kwargs if arguments is None else arguments)
    return call


web = types.ModuleType('web')
web.run = _host_function('web__run')

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
                  display=display, text=text, notify=notify, set_checkin=set_checkin,
                  web=web,
                  image=image, asyncio=asyncio, pathlib=pathlib, Path=pathlib.Path)


def _configure_tools(specs):
    agents = types.ModuleType('agents')

    def message(*, agent_id, message):
        return _request('message_agent', dict(agent_id=agent_id, message=message))

    def cancel(*, engineer_id):
        return _request('interrupt_engineer', dict(engineer_id=engineer_id))

    def spawn_new_advisor(msg):
        return _request('ask_advisor', dict(message=msg))

    def delegate_engineer(*, task_name, prompt, workdirs=None):
        arguments = dict(task_name=task_name, prompt=prompt)
        if workdirs is not None:
            arguments['workdirs'] = workdirs
        return _request('spawn_engineer', arguments)

    agent_functions = {
        'message_agent': message,
        'interrupt_engineer': cancel,
        'ask_advisor': spawn_new_advisor,
        'spawn_engineer': delegate_engineer,
    }
    for spec in specs:
        name = spec['name']
        if name in agent_functions:
            function = agent_functions[name]
            function.__module__ = 'agents'
            schema = spec['input_schema']
            if name == 'ask_advisor' and 'message' in schema.get('properties', {}):
                schema['properties']['msg'] = schema['properties'].pop('message')
                schema['required'] = ['msg']
            function.__doc__ = spec['description'] + '\nArguments: ' + json.dumps(schema)
            setattr(agents, function.__name__, function)
        elif name != 'web__run':
            _namespace[name] = _host_function(name)
    _namespace['agents'] = agents
    sys.modules['agents'] = agents


_main = types.ModuleType('__main__')
_main.__dict__.update(_namespace)
_namespace = _main.__dict__
sys.modules['__main__'] = _main
_configure_tools(json.loads(_tool_config))


async def _evaluate(cell, source):
    _send('started', cell=cell)
    try:
        if source is not None:
            code = compile(source, f'<rho-cell-{cell}>', 'exec', flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
            result = eval(code, _namespace)
            if isinstance(result, types.CoroutineType):
                await result
        else:
            stream = _streams[cell]
            eof = False
            while not stream['stopped']:
                fragment, stream['source'] = stream['source'], ''
                eof = eof or stream['eof']
                unit = _stream_next(cell, fragment, eof, False)
                if unit is None:
                    if eof:
                        break
                    stream['changed'].clear()
                    await stream['changed'].wait()
                    continue
                end, code = unit
                permit = stream['permit'] = _loop.create_future()
                stream['end'] = end
                _send('unit_ready', cell=cell, end=end)
                if not await permit:
                    break
                try:
                    result = eval(code, _namespace)
                    if isinstance(result, types.CoroutineType):
                        await result
                except BaseException as exc:
                    _send('unit_settled', cell=cell, end=end, error=_format_error(exc))
                    raise
                else:
                    _send('unit_settled', cell=cell, end=end, error=None)
    except BaseException as exc:
        _send('returned', cell=cell, error=_format_error(exc))
        raise
    else:
        _send('returned', cell=cell, error=None)
    finally:
        if source is None:
            _streams.pop(cell, None)
            _stream_next(cell, '', False, True)


def _execute(cell, source):
    if len(_cells) >= 128 or cell in _cells:
        _send('finished', cell=cell, error='Notebook live cell limit reached or duplicate cell')
        return
    if source is None:
        _streams[cell] = dict(source='', eof=False, stopped=False,
                              changed=asyncio.Event(), permit=None, end=0)
    _cells[cell] = dict(root=None, tasks=set(), errors=[], error=None, cancelled=False, workers=set())
    context = contextvars.copy_context()
    context.run(_cell.set, cell)
    try:
        _cells[cell]['root'] = _loop.create_task(_evaluate(cell, source), context=context)
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
        elif kind == 'begin_stream':
            _execute(message['cell'], None)
        elif kind == 'stream_feed':
            stream = _streams.get(message['cell'])
            if stream is not None and not stream['stopped']:
                stream['source'] += message['source']
                stream['eof'] = message['eof']
                stream['changed'].set()
        elif kind == 'stream_permit':
            stream = _streams.get(message['cell'])
            if stream is not None and stream['end'] == message['end']:
                permit = stream['permit']
                if permit is not None and not permit.done():
                    permit.set_result(not stream['stopped'])
        elif kind == 'stream_stop':
            stream = _streams.get(message['cell'])
            if stream is not None:
                stream['stopped'] = True
                stream['changed'].set()
                permit = stream['permit']
                if permit is not None and not permit.done():
                    permit.set_result(False)
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
            _cancel_requested(cell, True)
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
    threading.settrace(_trace)
    _heartbeat()
    try:
        _loop.run_forever()
    finally:
        _heartbeat_handle.cancel()
        for state in _cells.values():
            state['cancelled'] = True
        for task in asyncio.all_tasks(_loop):
            task.cancel()
        async def shutdown():
            await asyncio.gather(*asyncio.all_tasks(_loop) - {asyncio.current_task()},
                                 return_exceptions=True)
            await _loop.shutdown_asyncgens()
            await _loop.shutdown_default_executor()
            while any(state['workers'] for state in _cells.values()):
                await asyncio.sleep(0.01)
        _loop.run_until_complete(shutdown())
        sys.settrace(None)
        threading.settrace(None)
        _loop.close()
