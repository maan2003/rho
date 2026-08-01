use std::{
    io,
    num::NonZeroUsize,
    sync::{LockResult, Mutex, MutexGuard, TryLockError},
};

use wasm_bindgen::prelude::*;
use web_sys::{Blob, Url, WorkerGlobalScope};

pub fn available_parallelism() -> io::Result<NonZeroUsize> {
    if let Some(window) = web_sys::window() {
        return Ok(NonZeroUsize::new(window.navigator().hardware_concurrency() as usize).unwrap());
    }

    if let Ok(worker) = js_sys::global().dyn_into::<WorkerGlobalScope>() {
        return Ok(NonZeroUsize::new(worker.navigator().hardware_concurrency() as usize).unwrap());
    }

    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hardware_concurrency unsupported",
    ))
}

pub fn is_web_worker_thread() -> bool {
    js_sys::global().dyn_into::<WorkerGlobalScope>().is_ok()
}

/// Generates worker entry script as URL encoded blob
pub fn get_worker_script(wasm_bindgen_shim_url: Option<String>) -> String {
    // Cache URL so that subsequent calls are less expensive
    static CACHED_URL: Mutex<Option<String>> = Mutex::new(None);

    if let Some(url) = CACHED_URL.lock_spin().unwrap().clone() {
        return url;
    }

    let wasm_bindgen_shim_url = wasm_bindgen_shim_url
        .expect("the wasm-bindgen shim URL must be configured before spawning a worker");

    // Generate script from template
    #[cfg(feature = "es_modules")]
    let template = include_str!("js/web_worker_module.js");
    #[cfg(not(feature = "es_modules"))]
    let template = include_str!("js/web_worker.js");

    let script = template.replace("WASM_BINDGEN_SHIM_URL", &wasm_bindgen_shim_url);

    // Create url encoded blob
    let arr = js_sys::Array::new();
    arr.set(0, JsValue::from_str(&script));
    let blob = Blob::new_with_str_sequence(&arr).unwrap();
    let url = Url::create_object_url_with_blob(
        &blob
            .slice_with_f64_and_f64_and_content_type(0.0, blob.size(), "text/javascript")
            .unwrap(),
    )
    .unwrap();

    *CACHED_URL.lock_spin().unwrap() = Some(url.clone());

    url
}

/// A spin lock mutex extension.
///
/// Atomic wait panics in wasm main thread so we can't use `Mutex::lock()`.
/// This is a helper, which implement spinlock by calling `Mutex::try_lock()` in a loop.
/// Care must be taken not to introduce deadlocks when using this trait.
pub trait SpinLockMutex {
    type Inner;

    fn lock_spin<'a>(&'a self) -> LockResult<MutexGuard<'a, Self::Inner>>;
}

impl<T> SpinLockMutex for Mutex<T> {
    type Inner = T;

    fn lock_spin<'a>(&'a self) -> LockResult<MutexGuard<'a, Self::Inner>> {
        loop {
            match self.try_lock() {
                Ok(guard) => break Ok(guard),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Poisoned(e)) => break Err(e),
            }
        }
    }
}
