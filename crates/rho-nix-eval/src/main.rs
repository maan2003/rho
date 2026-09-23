//! Evaluates flake development shells for rho through the Nix C API.
//!
//! This binary links libnix and therefore runs apart from the (musl) rho
//! processes. For now it only has a command-line mode used to exercise the
//! evaluator and cache:
//!
//!     rho-nix-eval shell <flake-dir> [--shell NAME] [--cache DB] [--no-cache]

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use devenv_eval_cache::{
    CachingEvalService, Checkout, EvalCacheKey, FlakeScheme, any_input_modified_after,
    ops_to_identities,
};
use devenv_nix_backend::{DevShellRequest, NIX_STACK_SIZE, NixRuntime};

/// Changes whenever evaluation semantics change (evaluator, Nix fork patches).
const EVALUATOR: &str = concat!("rho-nix-eval/", env!("CARGO_PKG_VERSION"), " nix-2.35-rho");

struct Args {
    flake_dir: PathBuf,
    shell: String,
    cache: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("shell") {
        bail!("usage: rho-nix-eval shell <flake-dir> [--shell NAME] [--cache DB] [--no-cache]");
    }
    let flake_dir = args.next().context("missing flake directory")?;
    let flake_dir = std::fs::canonicalize(&flake_dir).with_context(|| flake_dir.clone())?;
    let mut parsed = Args {
        flake_dir,
        shell: "default".into(),
        cache: Some(default_cache_path()),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shell" => parsed.shell = args.next().context("--shell needs a value")?,
            "--cache" => parsed.cache = Some(args.next().context("--cache needs a value")?.into()),
            "--no-cache" => parsed.cache = None,
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(parsed)
}

fn default_cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    base.join("rho/nix-eval.sqlite")
}

fn main() -> Result<()> {
    let args = parse_args()?;
    // Evaluation recurses deeply; match the Nix CLI's stack.
    std::thread::Builder::new()
        .stack_size(NIX_STACK_SIZE)
        .spawn(move || run(args))?
        .join()
        .expect("evaluator thread panicked")
}

/// What rho keeps of an evaluated shell; cached as JSON.
struct Shell {
    drv_path: String,
    env_store_path: String,
    env_json: String,
}

impl Shell {
    fn to_json(&self) -> String {
        serde_json::json!({
            "drv_path": self.drv_path,
            "env_store_path": self.env_store_path,
            "env_json": self.env_json,
        })
        .to_string()
    }

    fn from_json(json: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let field = |name: &str| -> Result<String> {
            Ok(value[name].as_str().context(format!("cached shell lacks {name}"))?.to_owned())
        };
        Ok(Self {
            drv_path: field("drv_path")?,
            env_store_path: field("env_store_path")?,
            env_json: field("env_json")?,
        })
    }
}

fn run(args: Args) -> Result<()> {
    let t0 = Instant::now();
    let system = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let env = |name: &str| std::env::var(name).ok();
    let scheme = if args.flake_dir.join(".git").exists() {
        FlakeScheme::Git
    } else {
        FlakeScheme::Path
    };
    let lock = std::fs::read(args.flake_dir.join("flake.lock")).ok();
    let key = EvalCacheKey::new(
        &format!("devShells.{system}.{}", args.shell),
        scheme,
        &[EVALUATOR.as_bytes(), lock.as_deref().unwrap_or(b"\0no flake.lock")],
    );

    // A broken cache must not stop evaluation.
    let mut cache = args.cache.and_then(|path| {
        CachingEvalService::open(path)
            .inspect_err(|e| eprintln!("eval cache unavailable: {e}"))
            .ok()
    });
    if let Some(cache) = &cache {
        let hit = Checkout::new(&args.flake_dir, scheme)
            .map_err(anyhow::Error::from)
            .and_then(|checkout| Ok(cache.get_cached(&key, &checkout, &env)?));
        match hit {
            Ok(Some(hit)) => {
                report("hit", &Shell::from_json(&hit.json_output)?, t0, None);
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => eprintln!("eval cache lookup failed, evaluating: {e}"),
        }
    }
    let lookup_done = t0.elapsed();

    let mut nix = NixRuntime::new().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let request = DevShellRequest {
        flake_dir: args.flake_dir.clone(),
        system,
        shell: args.shell.clone(),
    };
    let eval = nix
        .eval_dev_shell(&request)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let shell = Shell {
        drv_path: eval.shell.drv_path,
        env_store_path: eval.shell.env_store_path,
        env_json: eval.shell.env_json,
    };
    let eval_done = t0.elapsed();

    let (fetched_as, identities) = ops_to_identities(&eval.ops, &args.flake_dir)?;
    if fetched_as != scheme {
        bail!("flake was fetched as {fetched_as:?}, expected {scheme:?}");
    }
    let checkout = Checkout::new(&args.flake_dir, scheme)?;
    let inputs = match &cache {
        Some(cache) => identities.to_inputs(&checkout, &mut cache.hashes(), &env)?,
        None => identities.to_inputs(&checkout, &mut devenv_eval_cache::eval_inputs::Uncached, &env)?,
    };
    for input in &inputs {
        eprintln!("input {input:?}");
    }
    let stats = Some((lookup_done, eval_done, inputs.len(), eval.ops.len()));
    if let Some(changed) = any_input_modified_after(&inputs, &checkout, eval.started_at) {
        eprintln!("not caching: {} changed during evaluation", changed.display());
        report("uncacheable", &shell, t0, stats);
        return Ok(());
    }
    if let Some(cache) = &mut cache {
        if let Err(e) = cache.store(&key, &shell.to_json(), &inputs) {
            eprintln!("failed to store eval result: {e}");
        }
    }
    report("miss", &shell, t0, stats);
    Ok(())
}

fn report(
    outcome: &str,
    shell: &Shell,
    t0: Instant,
    eval: Option<(std::time::Duration, std::time::Duration, usize, usize)>,
) {
    let mut out = serde_json::json!({
        "outcome": outcome,
        "drv_path": shell.drv_path,
        "env_store_path": shell.env_store_path,
        "env_json_bytes": shell.env_json.len(),
        "total_s": t0.elapsed().as_secs_f64(),
    });
    if let Some((lookup, eval, inputs, ops)) = eval {
        out["lookup_s"] = lookup.as_secs_f64().into();
        out["eval_s"] = (eval - lookup).as_secs_f64().into();
        out["inputs"] = inputs.into();
        out["effects"] = ops.into();
    }
    println!("{out}");
}
