//! Evaluate a flake's `devShells` output into a development environment.
//!
//! One [`NixRuntime`] owns the process-global Nix state: GC registration,
//! settings, the store connection and the logger. Each
//! [`NixRuntime::eval_dev_shell`] call builds a fresh `EvalState`, locks the
//! flake as `nix develop` does (writing `flake.lock` when it is missing or
//! stale), builds the shell's environment, and returns it, or its failure,
//! together with everything evaluation observed of local inputs meanwhile.
//! Callers decide what those observations mean for caching.

use std::path::{Path, PathBuf};

use miette::{Result, WrapErr, miette};
use nix_bindings_expr::eval_state::{EvalState, EvalStateBuilder, ThreadRegistrationGuard, gc_register_my_thread};
use nix_bindings_fetchers::FetchersSettings;
use nix_bindings_flake::{
    EvalStateBuilderExt, FlakeLockFlags, FlakeReference, FlakeReferenceParseFlags, FlakeSettings,
    LockedFlake,
};
use nix_bindings_store::build_env::BuildEnvironment as NixBuildEnvironment;
use nix_bindings_store::store::Store;
use nix_bindings_util::settings;

use crate::anyhow_ext::AnyhowToMiette;
use crate::gc_root::{GcRootOutcome, ensure_gc_root};
use crate::logger::setup_nix_logger;
use crate::observations::{LocalInput, Observation, observations};
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

/// A development shell's environment.
#[derive(Clone, Debug)]
pub struct DevShell {
    pub drv_path: String,
    /// Store path of the `-env` JSON produced by `getDevEnvironment`. Rooting
    /// it keeps the shell's inputs alive.
    pub env_store_path: String,
}

/// A development shell, or why there is none, plus what its evaluation
/// observed: up to the failure if it failed.
pub struct DevShellEval {
    pub shell: Result<DevShell>,
    pub observations: Vec<Observation>,
}

/// Process-wide Nix state. Must be created and used on one thread with a
/// large stack (see [`crate::NIX_STACK_SIZE`]).
pub struct NixRuntime {
    store: Store,
    flake_settings: FlakeSettings,
    fetchers_settings: FetchersSettings,
    _logger: nix_bindings_expr::logger::ActivityLogger,
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
        // Evaluation is under the hood, and checks open checkouts often.
        fetchers_settings
            .set("warn-dirty", "false")
            .to_miette()
            .wrap_err("Failed to set warn-dirty")?;
        let logger = setup_nix_logger()?;
        Ok(Self {
            store,
            flake_settings,
            fetchers_settings,
            _logger: logger,
            _gc: gc,
        })
    }

    /// Evaluate `request`'s shell and build its environment, recording what
    /// evaluation observed of local inputs.
    pub fn eval_dev_shell(&mut self, request: &DevShellRequest) -> Result<DevShellEval> {
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
        let shell = self.dev_shell(request, flake_dir, &mut state);
        Ok(DevShellEval {
            shell,
            observations: observations(&state)?,
        })
    }

    fn dev_shell(&mut self, request: &DevShellRequest, flake_dir: &str, state: &mut EvalState) -> Result<DevShell> {
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
        // As `nix develop`: lock what is unlocked, and write the lock file.
        lock.set_mode_write_as_needed().to_miette()?;
        let locked = LockedFlake::lock(
            &self.fetchers_settings,
            &self.flake_settings,
            state,
            &lock,
            &flake_ref,
        )
        .to_miette()
        .wrap_err("Failed to lock flake")?;

        let outputs = locked
            .outputs(&self.flake_settings, state)
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
        // Building the environment needs the derivation, not its output.
        let drv_store_path = self.store.parse_store_path(&drv_path).to_miette()?;
        let (_, env_store_path) = {
            let _umask = UmaskGuard::restrictive();
            NixBuildEnvironment::get_dev_environment(&self.store, &drv_store_path)
                .to_miette()
                .wrap_err("Failed to get dev environment")?
        };
        let env_store_path = self.store.real_path(&env_store_path).to_miette()?;
        Ok(DevShell {
            drv_path,
            env_store_path,
        })
    }

    /// A local input, named as observations name it, to observe as
    /// evaluation would now.
    pub fn open_local_input(&self, url: &str) -> Result<LocalInput> {
        LocalInput::open(&self.fetchers_settings, &self.store, url)
    }

    /// Bash applying the environment `env_json` (the `-env` output) to an
    /// interactive shell exactly as `nix develop` does, `shellHook` last.
    /// Structured attributes files are written to `tmp_dir`, which uses of
    /// the script need; the environment's outputs go to `outputs_dir`.
    pub fn rc_script(&self, env_json: &str, tmp_dir: &Path, outputs_dir: &Path) -> Result<String> {
        crate::observations::rc_script(env_json, tmp_dir, outputs_dir)
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

/// Load nix.conf into `builder`, then force the settings evaluation caching
/// depends on: pure evaluation, so the shell depends only on its flake as with
/// `nix develop`, and read recording, so every read of local inputs is
/// recorded with what it observed.
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
        result?;
        // Evaluation is under the hood; a dirty checkout is the norm.
        let view = check_call!(raw::eval_state_builder_fetch_settings_as_abstract_settings(
            &mut context,
            builder.raw_ptr()
        ))
        .to_miette()?;
        let result = check_call!(raw::abstract_settings_set(
            &mut context,
            view,
            c"warn-dirty".as_ptr(),
            c"false".as_ptr()
        ))
        .map(drop)
        .to_miette()
        .wrap_err("Failed to set warn-dirty");
        raw::abstract_settings_free(view);
        result
    }
}
