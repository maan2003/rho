use std::collections::HashSet;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use rustpython_vm::{Interpreter, PyResult, VirtualMachine};

use crate::{CellId, Event, Input, MAX_MESSAGE_BYTES};

pub(super) fn spawn(
    input: mpsc::Receiver<Input>,
    events: tokio::sync::mpsc::Sender<Event>,
    cancelled: Arc<Mutex<HashSet<CellId>>>,
    shutdown: Arc<AtomicBool>,
    space: Arc<tokio::sync::Notify>,
    wake: Arc<OwnedFd>,
    setup: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("rho-python".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let ended = events.clone();
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
                setup()?;
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
                    let ev = events.clone();
                    let emit = vm.new_function(
                        "_emit",
                        move |encoded: String, vm: &VirtualMachine| -> PyResult<()> {
                            if encoded.len() > MAX_MESSAGE_BYTES {
                                return Err(vm.new_value_error("Python output event exceeds 1 MiB"));
                            }
                            let event = serde_json::from_str(&encoded).map_err(|e| {
                                vm.new_value_error(format!("invalid runtime event: {e}"))
                            })?;
                            ev.blocking_send(event)
                                .map_err(|_| vm.new_runtime_error("Python host disconnected"))
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
                    let checkpoint = vm.new_function("_cancel_requested", move |cell: u64| {
                        shutdown.load(Ordering::Acquire) || cancelled.lock().unwrap().remove(&cell)
                    });
                    for (name, function) in [
                        ("_emit", emit),
                        ("_receive", receive),
                        ("_cancel_requested", checkpoint),
                    ] {
                        scope
                            .globals
                            .set_item(name, function.into(), vm)
                            .map_err(|e| format_exception(vm, e))?;
                    }
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
            let _ = ended.blocking_send(Event::Stopped { error });
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
