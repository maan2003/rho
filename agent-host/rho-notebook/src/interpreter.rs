//! The process's one CPython interpreter.
//!
//! A dedicated thread initializes it and then parks for the life of the
//! process holding no lock, so Python's main thread never exits under a
//! notebook. Notebook threads attach to the interpreter as ordinary threads.
use std::ffi::{CStr, c_char, c_int};
use std::sync::{OnceLock, mpsc};

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyModule;

#[repr(C)]
struct PyInitConfig {
    _private: [u8; 0],
}

// PEP 741 (Python 3.14): configuration by option name, independent of the
// `PyConfig` struct layout.
unsafe extern "C" {
    fn PyInitConfig_Create() -> *mut PyInitConfig;
    fn PyInitConfig_Free(config: *mut PyInitConfig);
    fn PyInitConfig_GetError(config: *mut PyInitConfig, message: *mut *const c_char) -> c_int;
    fn PyInitConfig_SetInt(config: *mut PyInitConfig, name: *const c_char, value: i64) -> c_int;
    fn Py_InitializeFromInitConfig(config: *mut PyInitConfig) -> c_int;
}

const OPTIONS: [(&CStr, i64); 3] = [
    // Importing subprocess/signal must not replace the embedding host's
    // Ctrl-C handler. Explicit Python signal changes remain ordinary.
    (c"install_signal_handlers", 0),
    (c"utf8_mode", 1),
    // Threads start in a copy of their creator's context, and so in its cell.
    (c"thread_inherit_context", 1),
];

/// Start the interpreter once per process.
pub(crate) fn initialize() -> Result<(), String> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            let (started, result) = mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("rho-python-main".into())
                .spawn(move || {
                    let result = unsafe { start() };
                    if result.is_err() {
                        let _ = started.send(result);
                        return;
                    }
                    let _ = started.send(result);
                    loop {
                        std::thread::park();
                    }
                })
                .map_err(|error| format!("start Python: {error}"))?;
            result
                .recv()
                .map_err(|_| "Python startup thread exited".to_owned())?
        })
        .clone()
}

unsafe fn start() -> Result<(), String> {
    unsafe {
        if pyo3::ffi::Py_IsInitialized() != 0 {
            return Err("Python was initialized outside rho's notebook runtime".into());
        }
        let config = PyInitConfig_Create();
        if config.is_null() {
            return Err("Python configuration: out of memory".into());
        }
        let mut ok = OPTIONS
            .iter()
            .all(|(name, value)| PyInitConfig_SetInt(config, name.as_ptr(), *value) == 0);
        ok = ok && Py_InitializeFromInitConfig(config) == 0;
        let result = if ok {
            Ok(())
        } else {
            let mut message = std::ptr::null();
            PyInitConfig_GetError(config, &mut message);
            Err(if message.is_null() {
                "Python initialization failed".into()
            } else {
                format!(
                    "Python initialization: {}",
                    CStr::from_ptr(message).to_string_lossy()
                )
            })
        };
        PyInitConfig_Free(config);
        if result.is_ok() {
            // Release the interpreter lock; this thread keeps its state.
            pyo3::ffi::PyEval_SaveThread();
        }
        result
    }
}

/// The notebook kernel module, loaded once.
pub(crate) fn kernel(py: Python<'_>) -> PyResult<&Bound<'_, PyModule>> {
    static KERNEL: PyOnceLock<Py<PyModule>> = PyOnceLock::new();
    KERNEL
        .get_or_try_init(py, || {
            let sys_path = py.import("sys")?.getattr("path")?;
            sys_path.call_method1("insert", (0, env!("RHO_PYTHON_SITE_PACKAGES")))?;
            sys_path.call_method1("insert", (0, ""))?;
            // The selected upstream ghapi sources live in the binary, not on
            // the agent's filesystem. Register the package before its relative
            // imports, so `from ghapi.all import GhApi` is ordinary Python.
            let modules = py.import("sys")?.getattr("modules")?;
            let package = PyModule::new(py, "ghapi")?;
            package.setattr("__path__", Vec::<String>::new())?;
            modules.set_item("ghapi", &package)?;
            for (name, filename, source) in [
                (
                    "ghapi.gh_spec",
                    c"ghapi/gh_spec.py",
                    pyo3::ffi::c_str!(include_str!("ghapi/gh_spec.py")),
                ),
                (
                    "ghapi.core",
                    c"ghapi/core.py",
                    pyo3::ffi::c_str!(include_str!("ghapi/core.py")),
                ),
                (
                    "ghapi.all",
                    c"ghapi/all.py",
                    pyo3::ffi::c_str!(include_str!("ghapi/all.py")),
                ),
            ] {
                let module = PyModule::from_code(
                    py,
                    source,
                    filename,
                    std::ffi::CString::new(name)?.as_c_str(),
                )?;
                modules.set_item(name, &module)?;
                package.setattr(name.rsplit('.').next().unwrap(), &module)?;
            }
            let init = PyModule::from_code(
                py,
                pyo3::ffi::c_str!(include_str!("ghapi/__init__.py")),
                c"ghapi/__init__.py",
                c"ghapi",
            )?;
            package.setattr("GhApi", init.getattr("GhApi")?)?;
            package.setattr("APIError", init.getattr("APIError")?)?;
            PyModule::from_code(
                py,
                pyo3::ffi::c_str!(include_str!("kernel.py")),
                c"rho_kernel.py",
                c"rho_kernel",
            )
            .map(Bound::unbind)
        })
        .map(|kernel| kernel.bind(py))
}
