use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use rustpython_vm::builtins::PyDictRef;
use rustpython_vm::function::{ArgIntoFloat, OptionalArg};
use rustpython_vm::{AsObject, Interpreter, PyObjectRef, PyResult, TryFromObject, VirtualMachine};

use crate::{Cancellation, CellId, Event, Input, MAX_MESSAGE_BYTES};

/// Account for cell-owned work after an ordinary asyncio iteration. Python
/// objects remain on the interpreter thread; scheduling and I/O stay in
/// asyncio.
fn settle_cells(
    event_loop: PyObjectRef,
    cell_context: PyObjectRef,
    cells: PyDictRef,
    format_error: PyObjectRef,
    emit: PyObjectRef,
    vm: &VirtualMachine,
) -> PyResult<()> {
    if cells.is_empty() {
        return Ok(());
    }
    let mut active = HashSet::new();
    let mut visit = |handle: PyObjectRef| -> PyResult<()> {
        // Like BaseEventLoop itself, inspect its Handle fields directly.
        if vm.is_none(&handle) || handle.get_attr("_cancelled", vm)?.try_to_bool(vm)? {
            return Ok(());
        }
        let context = handle.get_attr("_context", vm)?;
        let cell: Option<u64> = vm
            .call_method(&context, "get", (cell_context.clone(),))?
            .try_into_value(vm)?;
        if let Some(cell) = cell {
            active.insert(cell);
        }
        Ok(())
    };
    for name in ["_ready", "_scheduled"] {
        let handles = event_loop
            .get_attr(name, vm)?
            .get_iter(vm)?
            .into_iter::<PyObjectRef>(vm)
            .collect::<PyResult<Vec<_>>>()?;
        for handle in handles {
            visit(handle)?;
        }
    }
    // Avoid the Python Mapping/ValuesView iterator stack over the selector's
    // backing dict. SelectorEventLoop owns this standard selector implementation.
    let selector_keys: PyDictRef = event_loop
        .get_attr("_selector", vm)?
        .get_attr("_fd_to_key", vm)?
        .try_into_value(vm)?;
    for key in selector_keys.values_vec() {
        for handle in key
            .get_attr("data", vm)?
            .get_iter(vm)?
            .into_iter::<PyObjectRef>(vm)
        {
            visit(handle?)?;
        }
    }
    // Snapshot before deleting entries or invoking exception formatting.
    for (cell_key, state) in cells.items_vec() {
        let cell: u64 = cell_key.clone().try_into_value(vm)?;
        let state: PyDictRef = state.try_into_value(vm)?;
        if state.get_item("tasks", vm)?.try_to_bool(vm)?
            || state.get_item("workers", vm)?.try_to_bool(vm)?
            || active.contains(&cell)
        {
            continue;
        }
        let root = state.get_item("root", vm)?;
        if !vm.call_method(&root, "done", ())?.try_to_bool(vm)? {
            continue;
        }
        let mut error = state.get_item("error", vm)?;
        if vm.call_method(&root, "cancelled", ())?.try_to_bool(vm)? {
            if !error.clone().try_to_bool(vm)? {
                error = vm.ctx.new_str("CancelledError").into();
            }
        } else if !vm.is_none(vm.call_method(&root, "exception", ())?.as_object())
            && !error.clone().try_to_bool(vm)?
        {
            error = format_error.call((vm.call_method(&root, "exception", ())?,), vm)?;
        }
        let mut errors = Vec::new();
        if error.clone().try_to_bool(vm)? {
            errors.push(error);
        }
        for task in state
            .get_item("errors", vm)?
            .get_iter(vm)?
            .into_iter::<PyObjectRef>(vm)
        {
            let task = task?;
            if !task.is(&root) && task.get_attr("_log_traceback", vm)?.try_to_bool(vm)? {
                errors.push(format_error.call((vm.call_method(&task, "exception", ())?,), vm)?);
            }
        }
        cells.del_item(&*cell_key, vm)?;
        let error = if errors.is_empty() {
            vm.ctx.none()
        } else {
            vm.call_method(
                vm.ctx.new_str("\n").as_object(),
                "join",
                (vm.ctx.new_list(errors),),
            )?
        };
        let fields = vm.ctx.new_dict();
        fields.set_item("cell", cell_key, vm)?;
        fields.set_item(
            "error",
            if error.clone().try_to_bool(vm)? {
                error
            } else {
                vm.ctx.none()
            },
            vm,
        )?;
        emit.call(("finished", fields), vm)?;
    }
    Ok(())
}

fn field<T: TryFromObject>(fields: &PyDictRef, name: &str, vm: &VirtualMachine) -> PyResult<T> {
    fields.get_item(name, vm)?.try_into_value(vm)
}

/// Copy only the protocol's typed fields; arbitrary Python objects never
/// escape the interpreter thread. Tool arguments retain Python's JSON encoder
/// semantics, rather than introducing a second partial object serializer.
fn execution_event(kind: &str, fields: &PyDictRef, vm: &VirtualMachine) -> PyResult<Event> {
    let cell = field(fields, "cell", vm)?;
    Ok(match kind {
        "started" => Event::Started { cell },
        "unit_ready" => Event::UnitReady {
            cell,
            end: field(fields, "end", vm)?,
        },
        "unit_settled" => Event::UnitSettled {
            cell,
            end: field(fields, "end", vm)?,
            error: field(fields, "error", vm)?,
        },
        "returned" => Event::Returned {
            cell,
            error: field(fields, "error", vm)?,
        },
        "call" => {
            let arguments: String = field(fields, "arguments", vm)?;
            if arguments.len() > MAX_MESSAGE_BYTES {
                return Err(vm.new_value_error("Python output event exceeds 1 MiB"));
            }
            Event::Call {
                cell,
                request: field(fields, "request", vm)?,
                name: field(fields, "name", vm)?,
                arguments: serde_json::from_str(&arguments)
                    .map_err(|e| vm.new_value_error(format!("invalid tool arguments: {e}")))?,
            }
        }
        "text" => Event::Text {
            cell,
            text: field(fields, "text", vm)?,
            max_tokens: field(fields, "max_tokens", vm)?,
            important: field(fields, "important", vm)?,
        },
        "checkin" => Event::Checkin {
            cell,
            seconds: field(fields, "seconds", vm)?,
            wake_on_tools: field(fields, "wake_on_tools", vm)?,
        },
        "finished" => Event::Finished {
            cell,
            error: field(fields, "error", vm)?,
        },
        _ => return Err(vm.new_value_error("invalid execution event")),
    })
}

pub(super) fn spawn(
    input: mpsc::Receiver<Input>,
    executions: crate::Executions,
    cancelled: Arc<Mutex<HashMap<CellId, Cancellation>>>,
    shutdown: Arc<AtomicBool>,
    space: Arc<tokio::sync::Notify>,
    wake: Arc<OwnedFd>,
    setup: impl FnOnce() -> Result<serde_json::Value, String> + Send + 'static,
) -> Result<(), String> {
    static TLS: OnceLock<Result<(), String>> = OnceLock::new();
    TLS.get_or_init(|| {
        use rustpython_stdlib::ssl::providers::CryptoExt;
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        CryptoExt::set_ext(CryptoExt {
            all_cipher_suites: None,
            default_cipher_suites: None,
            all_kx_groups: None,
            any_supported_key: None,
            ticketer: rustls::crypto::aws_lc_rs::Ticketer::new,
        })
        .map_err(|error| format!("initialize Python TLS: {error}"))
    })
    .clone()?;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rho-python".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let ended = Arc::clone(&executions);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Filesystem state (not the process or descriptor table) is private
                // to this dedicated thread. Python chdir must not move the daemon
                // or another notebook. The caller then installs its workspace view.
                #[cfg(target_os = "linux")]
                if unsafe { libc::unshare(libc::CLONE_FS) } != 0 {
                    return Err(format!(
                        "unshare notebook cwd: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let tools = setup()?;
                // Importing subprocess/signal must not replace the embedding
                // host's Ctrl-C handler. Explicit Python signal changes remain
                // ordinary unsandboxed operations.
                let mut settings = rustpython_vm::Settings::default();
                settings.install_signal_handlers = false;
                // The standalone RustPython CLI resolves the default -1 to 1;
                // embedding skips that step. I/O checks > 0, unlike sys.flags.
                settings.utf8_mode = 1;
                let builder = Interpreter::builder(settings);
                let defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
                let interpreter = builder
                    .add_native_modules(&defs)
                    .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
                    .build();
                interpreter.enter(|vm| -> Result<(), String> {
                    let scope = vm.new_scope_with_builtins();
                    let execs = Arc::clone(&executions);
                    let finished_cancellations = Arc::clone(&cancelled);
                    let emit = vm.new_function(
                        "_emit",
                        move |kind: String,
                              fields: PyDictRef,
                              vm: &VirtualMachine|
                              -> PyResult<()> {
                            let event = execution_event(&kind, &fields, vm)?;
                            // The wire-size budget remains shared with incoming
                            // messages even though fixed events no longer make
                            // a JSON round trip through the interpreter.
                            let encoded = serde_json::to_vec(&event)
                                .map_err(|e| vm.new_value_error(e.to_string()))?;
                            if encoded.len() > MAX_MESSAGE_BYTES {
                                return Err(vm.new_value_error("Python output event exceeds 1 MiB"));
                            }
                            let cell = match &event {
                                Event::Started { cell }
                                | Event::UnitReady { cell, .. }
                                | Event::UnitSettled { cell, .. }
                                | Event::Returned { cell, .. }
                                | Event::Call { cell, .. }
                                | Event::Text { cell, .. }
                                | Event::Checkin { cell, .. }
                                | Event::Finished { cell, .. } => *cell,
                                Event::Stopped { .. } => {
                                    return Err(vm.new_value_error("invalid execution event"));
                                }
                            };
                            let finished = matches!(event, Event::Finished { .. });
                            let exec =
                                execs.lock().unwrap().get(&cell).cloned().ok_or_else(|| {
                                    vm.new_runtime_error("execution no longer exists")
                                })?;
                            exec.event(event);
                            if finished {
                                execs.lock().unwrap().remove(&cell);
                                finished_cancellations.lock().unwrap().remove(&cell);
                            }
                            Ok(())
                        },
                    );
                    let inbox = Mutex::new(input);
                    let stopping = Arc::clone(&shutdown);
                    let available = Arc::clone(&space);
                    let cancel_inbox = Arc::clone(&cancelled);
                    let fd = wake.as_raw_fd();
                    scope
                        .globals
                        .set_item("_inbox_fd", vm.ctx.new_int(fd).into(), vm)
                        .map_err(|e| format_exception(vm, e))?;
                    let receive = vm.new_function(
                        "_receive",
                        move |vm: &VirtualMachine| -> PyResult<String> {
                            let mut value = 0_u64;
                            unsafe {
                                libc::read(wake.as_raw_fd(), (&mut value as *mut u64).cast(), 8)
                            };
                            if stopping.load(Ordering::Acquire) {
                                return Ok("[{\"kind\":\"shutdown\"}]".into());
                            }
                            let rx = inbox.lock().unwrap();
                            let mut batch: Vec<_> = rx.try_iter().take(64).collect();
                            if batch.len() == 64 {
                                crate::wake(&wake);
                            }
                            available.notify_waiters();
                            batch.extend(
                                cancel_inbox
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter(|(_, state)| matches!(state, Cancellation::Pending))
                                    .map(|(&cell, _)| Input::Cancel { cell }),
                            );
                            serde_json::to_string(&batch)
                                .map_err(|e| vm.new_runtime_error(e.to_string()))
                        },
                    );
                    let checkpoint_cancelled = Arc::clone(&cancelled);
                    let checkpoint_shutdown = Arc::clone(&shutdown);
                    let cancel_executions = Arc::clone(&executions);
                    let checkpoint = vm.new_function(
                        "_cancel_requested",
                        move |cell: u64, acknowledge: bool| {
                            let mut cancelled = cancelled.lock().unwrap();
                            shutdown.load(Ordering::Acquire)
                                || if acknowledge {
                                    if !cancel_executions.lock().unwrap().contains_key(&cell) {
                                        cancelled.remove(&cell);
                                        false
                                    } else if let Some(state) = cancelled.get_mut(&cell) {
                                        *state = Cancellation::Delivered;
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    cancelled.contains_key(&cell)
                                }
                        },
                    );
                    // The checkpoint's normal path stays in Rust; only the
                    // notebook thread consumes this synchronous-work budget.
                    let owner = std::thread::current().id();
                    let checkpoint_deadline = Arc::new(Mutex::new(Instant::now()));
                    let deadline = Arc::clone(&checkpoint_deadline);
                    let watchdog = vm.new_function(
                        "_sync_watchdog",
                        move |reset: OptionalArg<ArgIntoFloat>,
                              vm: &VirtualMachine|
                              -> PyResult<bool> {
                            if std::thread::current().id() != owner {
                                return Ok(false);
                            }
                            let mut deadline = deadline.lock().unwrap();
                            let now = Instant::now();
                            if let OptionalArg::Present(seconds) = reset {
                                let duration = Duration::try_from_secs_f64(seconds.into())
                                    .map_err(|e| vm.new_value_error(e.to_string()))?;
                                *deadline = now + duration;
                                Ok(false)
                            } else {
                                Ok(now >= *deadline)
                            }
                        },
                    );
                    let compilers = Mutex::new(HashMap::<
                        u64,
                        rustpython_vm::compiler::StreamingCompiler,
                    >::new());
                    let stream_next = vm.new_function(
                        "_stream_next",
                        move |cell: u64,
                              fragment: String,
                              eof: bool,
                              close: bool,
                              vm: &VirtualMachine|
                              -> PyResult {
                            let mut compilers = compilers.lock().unwrap();
                            if close {
                                compilers.remove(&cell);
                                return Ok(vm.ctx.none());
                            }
                            let compiler = compilers.entry(cell).or_insert_with(|| {
                                rustpython_vm::compiler::StreamingCompiler::new(
                                    format!("<rho-cell-{cell}>"),
                                    rustpython_vm::compiler::CompileOpts {
                                        allow_top_level_await: true,
                                        ..vm.compile_opts()
                                    },
                                )
                            });
                            if !fragment.is_empty() {
                                compiler.feed(&fragment);
                            }
                            if eof {
                                compiler.finish();
                            }
                            match compiler
                                .next_unit()
                                .map_err(|error| vm.new_syntax_error(&error, None))?
                            {
                                Some(code) => Ok(vm
                                    .ctx
                                    .new_tuple(vec![
                                        vm.ctx.new_int(compiler.compiled_bytes()).into(),
                                        vm.ctx.new_code(code).into(),
                                    ])
                                    .into()),
                                None => Ok(vm.ctx.none()),
                            }
                        },
                    );
                    for (name, function) in [
                        ("_emit", emit),
                        (
                            "_settle_cells",
                            vm.new_function("_settle_cells", settle_cells),
                        ),
                        ("_stream_next", stream_next),
                        ("_receive", receive),
                        ("_cancel_requested", checkpoint),
                        ("_sync_watchdog", watchdog),
                    ] {
                        scope
                            .globals
                            .set_item(name, function.into(), vm)
                            .map_err(|e| format_exception(vm, e))?;
                    }
                    scope
                        .globals
                        .set_item("_tool_config", vm.ctx.new_str(tools.to_string()).into(), vm)
                        .map_err(|e| format_exception(vm, e))?;
                    scope
                        .globals
                        .set_item(
                            "_site_packages",
                            vm.ctx.new_str(env!("RHO_PYTHON_SITE_PACKAGES")).into(),
                            vm,
                        )
                        .map_err(|e| format_exception(vm, e))?;
                    let source = include_str!("notebook.py");
                    let code = vm
                        .compile(source, rustpython_vm::compiler::Mode::Exec, "<rho-runtime>")
                        .map_err(|e| e.to_string())?;
                    vm.run_code_obj(code, scope.clone())
                        .map_err(|e| format_exception(vm, e))?;
                    vm.sys_module
                        .set_attr(
                            "_rho_execution_checkpoint",
                            scope
                                .globals
                                .get_item("_execution_checkpoint", vm)
                                .map_err(|e| format_exception(vm, e))?,
                            vm,
                        )
                        .map_err(|e| format_exception(vm, e))?;
                    vm.set_execution_checkpoint(
                        std::num::NonZeroU32::new(1024).unwrap(),
                        move |vm| {
                            let timed_out = std::thread::current().id() == owner
                                && Instant::now() >= *checkpoint_deadline.lock().unwrap();
                            let cancel_pending = checkpoint_shutdown.load(Ordering::Acquire)
                                || !checkpoint_cancelled.lock().unwrap().is_empty();
                            if !timed_out && !cancel_pending {
                                return Ok(());
                            }
                            // Materialize frames only on pending interruption,
                            // before entering the Python eligibility check.
                            let Some(frame) = vm.current_frame() else {
                                return Ok(());
                            };
                            vm.sys_module
                                .get_attr("_rho_execution_checkpoint", vm)?
                                .call((frame, timed_out, cancel_pending), vm)
                                .map(|_| ())
                        },
                    );
                    let _ = ready_tx.send(Ok(()));
                    let code = vm
                        .compile(
                            "_run()",
                            rustpython_vm::compiler::Mode::Exec,
                            "<rho-runtime>",
                        )
                        .map_err(|e| e.to_string())?;
                    let result = vm.run_code_obj(code, scope);
                    vm.clear_execution_checkpoint();
                    result.map_err(|e| format_exception(vm, e))?;
                    Ok(())
                })
            }));
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some("RustPython interpreter panicked; notebook state lost".into()),
            };
            let _ = ready_tx.send(Err(error
                .clone()
                .unwrap_or_else(|| "Python runtime stopped during startup".into())));
            space.notify_waiters();
            let execs = std::mem::take(&mut *ended.lock().unwrap());
            for exec in execs.into_values() {
                exec.event(Event::Stopped {
                    error: error.clone(),
                });
            }
        })
        .map_err(|e| e.to_string())?;
    ready_rx
        .recv_timeout(Duration::from_secs(60))
        .map_err(|e| format!("Python startup failed: {e}"))?
}

fn format_exception(
    vm: &VirtualMachine,
    error: rustpython_vm::builtins::PyBaseExceptionRef,
) -> String {
    let mut text = String::new();
    if vm.write_exception(&mut text, &error).is_err() {
        return "Python interpreter error".into();
    }
    text
}
