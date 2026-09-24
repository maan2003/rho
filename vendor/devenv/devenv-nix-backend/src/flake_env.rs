//! Evaluate a flake's `devShells` output into a development environment.
//!
//! One [`NixRuntime`] owns the process-global Nix state: GC registration,
//! settings, the store connection and the logger. Each
//! [`NixRuntime::eval_dev_shell`] call builds a fresh `EvalState`, locks the
//! flake in check mode (a stale or missing lock is an error, never a write),
//! realises the shell derivation, and returns the environment together with
//! every evaluation effect observed while producing it. Callers decide what
//! those effects mean for caching.

use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use devenv_core::eval_op::{EvalOp, OpObserver};
use devenv_core::nix_log_bridge::NixLogBridge;
use miette::{Result, WrapErr, miette};
use nix_bindings_expr::eval_state::{EvalStateBuilder, ThreadRegistrationGuard, gc_register_my_thread};
use nix_bindings_fetchers::FetchersSettings;
use nix_bindings_flake::{
    EvalStateBuilderExt, FlakeLockFlags, FlakeReference, FlakeReferenceParseFlags, FlakeSettings,
    LockedFlake,
};
use nix_bindings_store::build_env::BuildEnvironment as NixBuildEnvironment;
use nix_bindings_store::store::Store;
use nix_bindings_util::settings;

use crate::anyhow_ext::AnyhowToMiette;
use crate::build_environment::BuildEnvironment;
use crate::gc_root::{GcRootOutcome, ensure_gc_root};
use crate::logger::{NixLoggerSetup, setup_nix_logger};
use crate::umask_guard::UmaskGuard;

/// Which development shell to evaluate.
#[derive(Clone, Debug)]
pub struct DevShellRequest {
    /// Directory containing `flake.nix`; the flake reference is `path` itself,
    /// so a git checkout is fetched as `git+file` and sees only tracked files.
    pub flake_dir: PathBuf,
    pub system: String,
    pub shell: String,
}

impl DevShellRequest {
    pub fn attr_path(&self) -> [&str; 3] {
        ["devShells", &self.system, &self.shell]
    }
}

/// A realised development shell.
#[derive(Clone, Debug)]
pub struct DevShell {
    pub drv_path: String,
    /// Store path of the `-env` JSON produced by `getDevEnvironment`. Rooting
    /// it keeps the shell's inputs alive.
    pub env_store_path: String,
    /// The environment as Nix serialised it, before any shell hook runs.
    pub env_json: String,
    pub env: BuildEnvironment,
}

/// A development shell plus what its evaluation depended on.
pub struct DevShellEval {
    pub shell: DevShell,
    /// Distinct effects, including input mounts, in no particular order.
    pub ops: Vec<EvalOp>,
}

/// Process-wide Nix state. Must be created and used on one thread with a
/// large stack (see [`crate::NIX_STACK_SIZE`]).
pub struct NixRuntime {
    store: Store,
    flake_settings: FlakeSettings,
    fetchers_settings: FetchersSettings,
    logger: NixLoggerSetup,
    _gc: ThreadRegistrationGuard,
}

impl NixRuntime {
    pub fn new() -> Result<Self> {
        crate::nix_init();
        let gc = gc_register_my_thread()
            .to_miette()
            .wrap_err("Failed to register thread with Nix garbage collector")?;
        settings::set("extra-experimental-features", "flakes nix-command")
            .to_miette()
            .wrap_err("Failed to enable experimental features")?;
        let store = Store::open(None, [])
            .to_miette()
            .wrap_err("Failed to open Nix store")?;
        let flake_settings = FlakeSettings::new()
            .to_miette()
            .wrap_err("Failed to create flake settings")?;
        let fetchers_settings = FetchersSettings::new()
            .to_miette()
            .wrap_err("Failed to create fetchers settings")?;
        let logger = setup_nix_logger()?;
        Ok(Self {
            store,
            flake_settings,
            fetchers_settings,
            logger,
            _gc: gc,
        })
    }

    pub fn log_bridge(&self) -> &Arc<NixLogBridge> {
        &self.logger.bridge
    }

    /// Evaluate and realise `request`'s shell, recording evaluation effects.
    pub fn eval_dev_shell(&mut self, request: &DevShellRequest) -> Result<DevShellEval> {
        let recorder = Arc::new(Recorder::default());
        let observer: Arc<dyn OpObserver> = recorder.clone();
        self.logger.bridge.add_observer(Arc::clone(&observer));
        let shell = self.eval_dev_shell_inner(request);
        self.logger.bridge.remove_observer(&observer);
        let ops = std::mem::take(&mut *recorder.ops.lock().unwrap())
            .into_iter()
            .collect();
        Ok(DevShellEval {
            shell: shell?,
            ops,
        })
    }

    fn eval_dev_shell_inner(&mut self, request: &DevShellRequest) -> Result<DevShell> {
        let flake_dir = request
            .flake_dir
            .to_str()
            .ok_or_else(|| miette!("flake directory is not UTF-8"))?;
        let builder = EvalStateBuilder::new(self.store.clone())
            .to_miette()?
            .flakes(&self.flake_settings)
            .to_miette()?
            .skip_load_config();
        configure_eval_settings(&builder)?;
        let mut state = builder
            .build()
            .to_miette()
            .wrap_err("Failed to build eval state")?;

        let mut parse = FlakeReferenceParseFlags::new(&self.flake_settings).to_miette()?;
        parse.set_base_directory(flake_dir).to_miette()?;
        let (flake_ref, _) = FlakeReference::parse_with_fragment(
            &self.fetchers_settings,
            &self.flake_settings,
            &parse,
            ".",
        )
        .to_miette()
        .wrap_err_with(|| format!("Failed to parse flake reference for {flake_dir}"))?;
        let mut lock = FlakeLockFlags::new(&self.flake_settings).to_miette()?;
        lock.set_mode_check().to_miette()?;
        let locked = LockedFlake::lock(
            &self.fetchers_settings,
            &self.flake_settings,
            &state,
            &lock,
            &flake_ref,
        )
        .to_miette()
        .wrap_err("Failed to lock flake (is flake.lock up to date?)")?;

        let outputs = locked
            .outputs(&self.flake_settings, &mut state)
            .to_miette()
            .wrap_err("Failed to evaluate flake outputs")?;
        let mut drv = outputs;
        for name in request.attr_path() {
            drv = state
                .require_attrs_select(&drv, name)
                .to_miette()
                .wrap_err_with(|| format!("flake has no {}", request.attr_path().join(".")))?;
        }
        state.force(&drv).to_miette()?;
        let drv_path = state.require_attrs_select(&drv, "drvPath").to_miette()?;
        let drv_path = state.require_string(&drv_path).to_miette()?;
        let out_path = state.require_attrs_select(&drv, "outPath").to_miette()?;
        {
            let _umask = UmaskGuard::restrictive();
            state
                .realise_string(&out_path, false)
                .to_miette()
                .wrap_err("Failed to realise shell derivation")?;
        }

        let drv_store_path = self.store.parse_store_path(&drv_path).to_miette()?;
        let (mut nix_env, env_store_path) = {
            let _umask = UmaskGuard::restrictive();
            NixBuildEnvironment::get_dev_environment(&self.store, &drv_store_path)
                .to_miette()
                .wrap_err("Failed to get dev environment")?
        };
        let env_json = nix_env.to_json().to_miette()?;
        let env = BuildEnvironment::from_json(&env_json)
            .map_err(|e| miette!("Failed to parse dev environment: {e}"))?;
        let env_store_path = self.store.real_path(&env_store_path).to_miette()?;
        Ok(DevShell {
            drv_path,
            env_store_path,
            env_json,
            env,
        })
    }

    /// Point `gc_root` at `store_path` and register it as a Nix GC root.
    pub fn add_gc_root(&mut self, gc_root: &Path, store_path: &str) -> Result<GcRootOutcome> {
        ensure_gc_root(&mut self.store, gc_root, store_path)
    }

    /// Root `store_path` at `gc_root` if it is still valid, and report
    /// whether it was. Nix registers a temporary root before the permanent
    /// one, so a garbage collection cannot invalidate the path after this
    /// checks it; a path already collected leaves no root behind.
    pub fn pin(&mut self, gc_root: &Path, store_path: &str) -> Result<bool> {
        use nix_bindings_bindgen_raw as raw;
        use nix_bindings_util::check_call;
        use nix_bindings_util::context::Context;

        ensure_gc_root(&mut self.store, gc_root, store_path)?;
        let path = self
            .store
            .parse_store_path(store_path)
            .to_miette()
            .wrap_err("Failed to parse store path")?;
        let mut context = Context::new();
        let valid = unsafe {
            check_call!(raw::store_is_valid_path(
                &mut context,
                self.store.raw_ptr(),
                path.as_ptr()
            ))
        }
        .to_miette()?;
        if !valid {
            std::fs::remove_file(gc_root)
                .map_err(|e| miette::miette!("Failed to remove GC root: {}", e))?;
        }
        Ok(valid)
    }
}

/// Collects distinct effects. Nix repeats many of them (`pathExists`,
/// `getEnv`, re-reads), so deduplicating on insert bounds memory by the
/// distinct inputs rather than the raw event count, as devenv's tracker does.
#[derive(Default)]
struct Recorder {
    ops: Mutex<HashSet<EvalOp>>,
}

impl OpObserver for Recorder {
    fn record(&self, op: EvalOp) {
        self.ops.lock().unwrap().insert(op);
    }
}

/// Load nix.conf into `builder`, then force the settings evaluation caching
/// depends on: pure evaluation, so the shell depends only on its flake as with
/// `nix develop`, and read recording, so every read of the flake's sources is
/// reported with what it observed.
fn configure_eval_settings(builder: &EvalStateBuilder) -> Result<()> {
    use nix_bindings_bindgen_raw as raw;
    use nix_bindings_util::check_call;
    use nix_bindings_util::context::Context;

    let mut context = Context::new();
    // SAFETY: the builder outlives the settings view, which is freed before
    // returning.
    unsafe {
        check_call!(raw::eval_state_builder_load(&mut context, builder.raw_ptr())).to_miette()?;
        let view = check_call!(raw::eval_state_builder_eval_settings_as_abstract_settings(
            &mut context,
            builder.raw_ptr()
        ))
        .to_miette()?;
        let mut result = Ok(());
        for (key, value) in [(c"pure-eval", c"true"), (c"record-input-reads", c"true")] {
            result = check_call!(raw::abstract_settings_set(&mut context, view, key.as_ptr(), value.as_ptr()))
                .map(drop)
                .to_miette()
                .wrap_err_with(|| format!("Failed to set {key:?}"));
            if result.is_err() {
                break;
            }
        }
        raw::abstract_settings_free(view);
        result
    }
}
