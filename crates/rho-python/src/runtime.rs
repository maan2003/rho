use std::collections::HashSet;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use rustpython_vm::function::{ArgIntoFloat, OptionalArg};
use rustpython_vm::{Interpreter, PyResult, VirtualMachine};

use crate::{CellId, Event, Input, MAX_MESSAGE_BYTES};

pub(super) fn spawn(
    input: mpsc::Receiver<Input>,
    executions: crate::Executions,
    cancelled: Arc<Mutex<HashSet<CellId>>>,
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
                    let emit = vm.new_function(
                        "_emit",
                        move |encoded: String, vm: &VirtualMachine| -> PyResult<()> {
                            if encoded.len() > MAX_MESSAGE_BYTES {
                                return Err(vm.new_value_error("Python output event exceeds 1 MiB"));
                            }
                            let event = serde_json::from_str(&encoded).map_err(|e| {
                                vm.new_value_error(format!("invalid runtime event: {e}"))
                            })?;
                            let cell = match &event {
                                Event::Started { cell }
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
                                    .copied()
                                    .map(|cell| Input::Cancel { cell }),
                            );
                            serde_json::to_string(&batch)
                                .map_err(|e| vm.new_runtime_error(e.to_string()))
                        },
                    );
                    let checkpoint = vm.new_function(
                        "_cancel_requested",
                        move |cell: u64, acknowledge: bool| {
                            let mut cancelled = cancelled.lock().unwrap();
                            shutdown.load(Ordering::Acquire)
                                || if acknowledge {
                                    cancelled.remove(&cell)
                                } else {
                                    cancelled.contains(&cell)
                                }
                        },
                    );
                    // Keep the trace hot path to one native call. Only the
                    // notebook thread consumes this synchronous-work budget.
                    let owner = std::thread::current().id();
                    let deadline = Mutex::new(Instant::now());
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
                    for (name, function) in [
                        ("_emit", emit),
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
                    let _ = ready_tx.send(Ok(()));
                    let code = vm
                        .compile(
                            "_run()",
                            rustpython_vm::compiler::Mode::Exec,
                            "<rho-runtime>",
                        )
                        .map_err(|e| e.to_string())?;
                    vm.run_code_obj(code, scope)
                        .map_err(|e| format_exception(vm, e))?;
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
