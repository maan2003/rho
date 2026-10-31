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
                    let result = unsafe { boot() };
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

unsafe fn boot() -> Result<(), String> {
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

/// Put the bundled site-packages on `sys.path`, once.
pub(crate) fn paths(py: Python<'_>) -> PyResult<()> {
    static PATHS: PyOnceLock<()> = PyOnceLock::new();
    PATHS
        .get_or_try_init(py, || {
            let sys_path = py.import("sys")?.getattr("path")?;
            let site_packages = rho_fs_view::bundled(
                "python-site-packages",
                option_env!("RHO_PYTHON_SITE_PACKAGES"),
            )
            .ok_or_else(|| {
                pyo3::exceptions::PyRuntimeError::new_err(
                    "no Python site-packages: build with RHO_PYTHON_SITE_PACKAGES \
                     or install them at ../share/rho/python-site-packages",
                )
            })?;
            // A str: the import system skips the `pathlib.Path` a PathBuf becomes.
            sys_path.call_method1("insert", (0, site_packages.to_string_lossy()))?;
            sys_path.call_method1("insert", (0, ""))?;
            Ok(())
        })
        .copied()
}

/// The notebook kernel module, loaded once.
pub(crate) fn kernel(py: Python<'_>) -> PyResult<&Bound<'_, PyModule>> {
    static KERNEL: PyOnceLock<Py<PyModule>> = PyOnceLock::new();
    KERNEL
        .get_or_try_init(py, || {
            paths(py)?;
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

/// Start the interpreter with the bundled packages importable, for a host
/// thread that runs Python outside a notebook.
pub fn start() -> Result<(), String> {
    initialize()?;
    Python::attach(|py| paths(py).map_err(|error| error.to_string()))
}
