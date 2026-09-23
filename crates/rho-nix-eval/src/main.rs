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
use devenv_eval_cache::{CacheKey, CachedShell, EnvCache, FlakeScheme, record_inputs};
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

fn run(args: Args) -> Result<()> {
    let t0 = Instant::now();
    let system = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let env = |name: &str| std::env::var(name).ok();
    let scheme = if args.flake_dir.join(".git").exists() {
        FlakeScheme::Git
    } else {
        FlakeScheme::Path
    };
    let key = CacheKey {
        system: system.clone(),
        shell: args.shell.clone(),
        scheme,
        lock: std::fs::read(args.flake_dir.join("flake.lock")).ok(),
        evaluator: EVALUATOR.into(),
    };

    let mut cache = args.cache.as_deref().map(EnvCache::open).transpose()?;
    if let Some(cache) = &cache {
        if let Some(hit) = cache.lookup(&key, &args.flake_dir, &env)? {
            report("hit", &hit, t0, None);
            return Ok(());
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
    let shell = CachedShell {
        drv_path: eval.shell.drv_path,
        env_store_path: eval.shell.env_store_path,
        env_json: eval.shell.env_json,
    };
    let eval_done = t0.elapsed();

    match record_inputs(&eval.ops, &args.flake_dir, eval.started_at, &env) {
        Ok(recorded) => {
            if recorded.scheme != scheme {
                bail!("flake was fetched as {:?}, expected {:?}", recorded.scheme, scheme);
            }
            for input in &recorded.inputs {
                eprintln!("input {:?} {:?} {} = {}", input.id.root, input.id.kind, input.id.path, input.state);
            }
            if let Some(cache) = &mut cache {
                cache.store(&key, &recorded.inputs, &shell)?;
            }
            report("miss", &shell, t0, Some((lookup_done, eval_done, recorded.inputs.len(), eval.ops.len())));
        }
        Err(e) => {
            eprintln!("not caching: {e}");
            report("uncacheable", &shell, t0, Some((lookup_done, eval_done, 0, eval.ops.len())));
        }
    }
    Ok(())
}

fn report(
    outcome: &str,
    shell: &CachedShell,
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
