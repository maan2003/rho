//! What evaluation observed of local inputs, and observing it again.
//!
//! With `record-input-reads`, the Nix fork records every read evaluation
//! makes of a local (not yet in the store) input: a path's type, a file's
//! contents, a directory's entries, a symlink's target, or a source-info
//! attribute such as `rev`. [`LocalInput::observe`] observes the same thing
//! again through the same code, so a result holds while every observation
//! it was built from compares equal.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::NonNull;

use miette::{Result, WrapErr, miette};
use nix_bindings_bindgen_raw as raw;
use nix_bindings_expr::eval_state::EvalState;
use nix_bindings_util::context::Context;
use nix_bindings_util::string_return::{callback_get_result_string, callback_get_result_string_data};
use nix_bindings_util::{check_call, result_string_init};

use crate::anyhow_ext::AnyhowToMiette;

/// One observation of a local input.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Observation {
    /// `stat`, `file`, `dir`, `link` or `attr`.
    pub kind: String,
    /// The input as evaluation named it, as a URL.
    pub input: String,
    /// Relative to the input's root, empty for the root; for `attr`, the
    /// attribute's name.
    pub path: String,
    /// What was observed; empty if the read failed.
    pub value: String,
}

/// What `state` observed so far, each distinct observation once. A path
/// observed with different values appears once per value.
pub(crate) fn observations(state: &EvalState) -> Result<Vec<Observation>> {
    unsafe extern "C" fn push(
        user_data: *mut c_void,
        kind: *const c_char,
        input: *const c_char,
        path: *const c_char,
        value: *const c_char,
    ) {
        let text = |s: *const c_char| unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
        let observations = unsafe { &mut *(user_data as *mut Vec<Observation>) };
        observations.push(Observation {
            kind: text(kind),
            input: text(input),
            path: text(path),
            value: text(value),
        });
    }
    let mut observations = Vec::new();
    let mut context = Context::new();
    // SAFETY: the callback only runs during the call, with `observations`
    // alive and borrowed by nothing else.
    unsafe {
        check_call!(raw::eval_state_input_observations(
            &mut context,
            state.raw_ptr(),
            Some(push),
            &mut observations as *mut Vec<Observation> as *mut c_void
        ))
    }
    .to_miette()
    .wrap_err("Failed to get input observations")?;
    Ok(observations)
}

/// A local input as evaluation would read it now.
pub struct LocalInput {
    ptr: NonNull<raw::local_input>,
}

impl LocalInput {
    pub(crate) fn open(
        fetchers: &nix_bindings_fetchers::FetchersSettings,
        store: &nix_bindings_store::store::Store,
        url: &str,
    ) -> Result<Self> {
        let c_url = CString::new(url).map_err(|_| miette!("input URL contains NUL: {url}"))?;
        let mut context = Context::new();
        // SAFETY: settings and store outlive the call; the input keeps what it
        // needs of them.
        let ptr = unsafe {
            check_call!(raw::local_input_open(
                &mut context,
                fetchers.raw_ptr(),
                store.raw_ptr(),
                c_url.as_ptr()
            ))
        }
        .to_miette()
        .wrap_err_with(|| format!("Failed to open input {url}"))?;
        let ptr = NonNull::new(ptr).ok_or_else(|| miette!("Failed to open input {url}"))?;
        Ok(Self { ptr })
    }

    /// What evaluation would observe now, in the format it records.
    pub fn observe(&mut self, kind: &str, path: &str) -> Result<String> {
        let kind = CString::new(kind).map_err(|_| miette!("observation kind contains NUL"))?;
        let path = CString::new(path).map_err(|_| miette!("observed path contains NUL"))?;
        let mut context = Context::new();
        let mut result = result_string_init!();
        // SAFETY: the callback only runs during the call.
        unsafe {
            check_call!(raw::local_input_observe(
                &mut context,
                self.ptr.as_ptr(),
                kind.as_ptr(),
                path.as_ptr(),
                Some(callback_get_result_string),
                callback_get_result_string_data(&mut result)
            ))
        }
        .to_miette()?;
        result.to_miette()
    }
}

impl Drop for LocalInput {
    fn drop(&mut self) {
        unsafe { raw::local_input_free(self.ptr.as_ptr()) }
    }
}

/// Bash applying the environment `env_json` (the `-env` output) to an
/// interactive shell exactly as `nix develop` does, `shellHook` last.
/// Structured attributes files are written to `tmp_dir`; the environment's
/// outputs are redirected into `outputs_dir`.
pub(crate) fn rc_script(env_json: &str, tmp_dir: &std::path::Path, outputs_dir: &std::path::Path) -> Result<String> {
    let json = CString::new(env_json).map_err(|_| miette!("environment contains NUL"))?;
    let path = |p: &std::path::Path| {
        CString::new(p.as_os_str().as_encoded_bytes()).map_err(|_| miette!("path contains NUL: {}", p.display()))
    };
    let (tmp_dir, outputs_dir) = (path(tmp_dir)?, path(outputs_dir)?);
    let mut context = Context::new();
    // SAFETY: the environment is freed before returning; the callback only
    // runs during the call.
    unsafe {
        let env = check_call!(raw::build_env_parse_json(&mut context, json.as_ptr()))
            .to_miette()
            .wrap_err("Failed to parse dev environment")?;
        if env.is_null() {
            return Err(miette!("Failed to parse dev environment"));
        }
        let mut result = result_string_init!();
        let called = check_call!(raw::build_env_to_rc_script(
            &mut context,
            env,
            tmp_dir.as_ptr(),
            outputs_dir.as_ptr(),
            Some(callback_get_result_string),
            callback_get_result_string_data(&mut result)
        ));
        raw::build_env_free(env);
        called.to_miette().wrap_err("Failed to write the dev environment's rc script")?;
        result.to_miette()
    }
}
