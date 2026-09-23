//! Builds flake development shells for rho through the Nix C API, and caches
//! the built environment against exactly what evaluation read.
//!
//! Evaluation is pure, and the Nix fork reports every read of the flake's
//! sources with what it observed, so a cached shell stays valid while those
//! observations still hold. This binary links libnix and therefore runs apart
//! from the (musl) rho processes. For now it only has a command-line mode used
//! to exercise the builder and cache:
//!
//!     rho-devshell-builder shell <flake-dir> [--shell NAME] [--cache DB] [--no-cache]

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use devenv_core::ObservedKind;
use devenv_eval_cache::{
    CachingEvalService, Checkout, EvalCacheKey, FlakeScheme, Input, RevInputDesc, record_inputs,
};
use devenv_nix_backend::build_environment::BuildEnvironment;
use devenv_nix_backend::{DevShellRequest, NIX_STACK_SIZE, NixRuntime};

/// Changes whenever evaluation semantics change (evaluator, Nix fork patches).
const EVALUATOR: &str = concat!("rho-devshell-builder/", env!("CARGO_PKG_VERSION"), " nix-2.35-rho2");

struct Args {
    flake_dir: PathBuf,
    shell: String,
    cache: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("shell") {
        bail!("usage: rho-devshell-builder shell <flake-dir> [--shell NAME] [--cache DB] [--no-cache]");
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
    base.join("rho/devshell-cache.sqlite")
}

fn main() -> Result<()> {
    let args = parse_args()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .without_time()
        .init();
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

/// Where the flake's source tree is fetched from, as Nix does for a local
/// flake: the enclosing git repository (with the flake in a subdirectory of
/// it), or the flake directory itself.
struct Source {
    scheme: FlakeScheme,
    root: PathBuf,
    /// The flake directory relative to `root`.
    subdir: PathBuf,
}

impl Source {
    fn find(flake_dir: &Path) -> Self {
        for dir in flake_dir.ancestors() {
            if dir.join(".git").exists() {
                return Self {
                    scheme: FlakeScheme::Git,
                    root: dir.to_path_buf(),
                    subdir: flake_dir.strip_prefix(dir).unwrap_or(Path::new("")).to_path_buf(),
                };
            }
        }
        Self {
            scheme: FlakeScheme::Path,
            root: flake_dir.to_path_buf(),
            subdir: PathBuf::new(),
        }
    }
}

fn run(args: Args) -> Result<()> {
    let t0 = Instant::now();
    let system = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let source = Source::find(&args.flake_dir);
    let lock = std::fs::read(args.flake_dir.join("flake.lock")).ok();
    let key = EvalCacheKey::new(
        &format!("devShells.{system}.{}", args.shell),
        source.scheme,
        &[
            EVALUATOR.as_bytes(),
            source.subdir.as_os_str().as_encoded_bytes(),
            lock.as_deref().unwrap_or(b"\0no flake.lock"),
        ],
    );

    // A broken cache must not stop evaluation.
    let mut cache = args.cache.as_ref().and_then(|path| {
        CachingEvalService::open(path.clone())
            .inspect_err(|e| eprintln!("eval cache unavailable: {e}"))
            .ok()
    });
    if let Some(cache) = &cache {
        match lookup(cache, &key, &source) {
            Ok(Some((shell, eval_id, inputs))) => {
                let watch = watch_paths(&source, &args.flake_dir, &inputs);
                return report("hit", &shell, Some(eval_id), &watch, t0, None);
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

    let recorded = match record_inputs(&eval.ops, &source.root) {
        Ok((fetched_as, _)) if fetched_as != source.scheme => {
            bail!("flake was fetched as {fetched_as:?}, expected {:?}", source.scheme)
        }
        Ok((_, recorded)) => recorded,
        Err(e) => {
            eprintln!("not caching: {e}");
            let stats = Some((lookup_done, eval_done, 0, eval.ops.len()));
            return report("uncacheable", &shell, None, &Watch::default(), t0, stats);
        }
    };
    let flake_rev = recorded.flake_rev;
    let mut inputs = recorded.into_inputs();
    if flake_rev {
        let checkout = Checkout::new(&source.root, source.scheme)?;
        inputs.push(Input::FlakeRev(RevInputDesc::new(&checkout)?));
    }
    let stats = Some((lookup_done, eval_done, inputs.len(), eval.ops.len()));
    let mut eval_id = None;
    if let (Some(cache), Some(cache_path)) = (&mut cache, &args.cache) {
        let stored = cache.store(&key, &shell.to_json(), &inputs).and_then(|eval_id| {
            Ok((eval_id, cache.eval_ids()?))
        });
        match stored {
            Ok((id, live)) => {
                eval_id = Some(id);
                let roots = gc_roots_dir(cache_path);
                if let Err(e) = root_shell(&mut nix, &roots, id, &shell, &live) {
                    eprintln!("failed to register GC root: {e}");
                }
            }
            Err(e) => eprintln!("failed to store eval result: {e}"),
        }
    }
    let watch = watch_paths(&source, &args.flake_dir, &inputs);
    report("miss", &shell, eval_id, &watch, t0, stats)
}

/// A valid cached shell for `key`, dropping candidates whose store paths were
/// garbage collected (as devenv does) and trying the next.
fn lookup(
    cache: &CachingEvalService,
    key: &EvalCacheKey,
    source: &Source,
) -> Result<Option<(Shell, i64, Vec<Input>)>> {
    let checkout = Checkout::new(&source.root, source.scheme)?;
    while let Some(hit) = cache.get_cached(key, &checkout)? {
        let shell = Shell::from_json(&hit.json_output)?;
        // Only the environment is used; its GC root keeps its closure alive.
        if Path::new(&shell.env_store_path).exists() {
            return Ok(Some((shell, hit.eval_id, hit.inputs)));
        }
        eprintln!("cached shell {} was garbage collected", shell.env_store_path);
        cache.remove(hit.eval_id)?;
    }
    Ok(None)
}

/// GC roots of cached shells live next to the cache, one per candidate.
fn gc_roots_dir(cache_path: &Path) -> PathBuf {
    let mut dir = cache_path.as_os_str().to_owned();
    dir.push(".gcroots");
    PathBuf::from(dir)
}

/// Root `shell`'s environment as candidate `eval_id`, and drop roots of
/// candidates the cache no longer holds.
fn root_shell(
    nix: &mut NixRuntime,
    roots: &Path,
    eval_id: i64,
    shell: &Shell,
    live: &[i64],
) -> Result<()> {
    std::fs::create_dir_all(roots)?;
    nix.add_gc_root(&roots.join(eval_id.to_string()), &shell.env_store_path)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    for entry in std::fs::read_dir(roots)? {
        let entry = entry?;
        let id = entry.file_name().to_str().and_then(|name| name.parse::<i64>().ok());
        if id.is_none_or(|id| !live.contains(&id)) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// What to watch to know a shell with `inputs` may be stale.
#[derive(Default)]
struct Watch {
    /// Paths whose contents matter: files read, directories listed,
    /// `flake.lock` (part of the key), and the git state deciding which files
    /// are visible and, if the shell used it, the revision.
    contents: std::collections::BTreeSet<PathBuf>,
    /// Paths only stat'd: replacing or deleting the entry matters, and so do
    /// its attributes, but not what a directory contains.
    names: std::collections::BTreeSet<PathBuf>,
}

fn watch_paths(source: &Source, flake_dir: &Path, inputs: &[Input]) -> Watch {
    let mut watch = Watch::default();
    watch.contents.insert(flake_dir.join("flake.lock"));
    let absolute = |rel: &Path| {
        if rel.as_os_str().is_empty() { source.root.clone() } else { source.root.join(rel) }
    };
    for input in inputs {
        match input {
            Input::Path(p) if p.kind == ObservedKind::Stat => {
                watch.names.insert(absolute(&p.path));
            }
            Input::Path(p) => {
                watch.contents.insert(absolute(&p.path));
            }
            Input::FlakeRev(_) => {
                if let Some(git) = GitDirs::find(&source.root) {
                    watch.contents.insert(git.dir.join("HEAD"));
                    watch.contents.insert(git.common.join("packed-refs"));
                    if let Some(head) = std::fs::read_to_string(git.dir.join("HEAD"))
                        .ok()
                        .and_then(|head| head.strip_prefix("ref: ").map(|r| r.trim().to_owned()))
                    {
                        watch.contents.insert(git.common.join(head));
                    }
                }
            }
        }
    }
    if source.scheme == FlakeScheme::Git
        && let Some(git) = GitDirs::find(&source.root)
    {
        watch.contents.insert(git.dir.join("index"));
    }
    watch.names.retain(|path| !watch.contents.contains(path));
    watch
}

/// A checkout's git directory, and the directory shared by its worktrees.
struct GitDirs {
    dir: PathBuf,
    common: PathBuf,
}

impl GitDirs {
    fn find(root: &Path) -> Option<Self> {
        let dot_git = root.join(".git");
        let dir = if dot_git.is_dir() {
            dot_git
        } else {
            let file = std::fs::read_to_string(&dot_git).ok()?;
            root.join(file.strip_prefix("gitdir:")?.trim())
        };
        let common = match std::fs::read_to_string(dir.join("commondir")) {
            Ok(common) => dir.join(common.trim()),
            Err(_) => dir.clone(),
        };
        Some(Self { dir, common })
    }
}

/// Print the result for the caller: the shell as a Bash activation script
/// (which runs `shellHook`), the candidate it is cached as, and what to
/// watch to know it may be stale.
fn report(
    outcome: &str,
    shell: &Shell,
    eval_id: Option<i64>,
    watch: &Watch,
    t0: Instant,
    eval: Option<(std::time::Duration, std::time::Duration, usize, usize)>,
) -> Result<()> {
    let activation = BuildEnvironment::from_json(&shell.env_json)?.to_activation_script();
    let mut out = serde_json::json!({
        "outcome": outcome,
        "eval_id": eval_id,
        "drv_path": shell.drv_path,
        "env_store_path": shell.env_store_path,
        "activation": activation,
        "watch": watch.contents,
        "watch_names": watch.names,
        "total_s": t0.elapsed().as_secs_f64(),
    });
    if let Some((lookup, eval, inputs, ops)) = eval {
        out["lookup_s"] = lookup.as_secs_f64().into();
        out["eval_s"] = (eval - lookup).as_secs_f64().into();
        out["inputs"] = inputs.into();
        out["effects"] = ops.into();
    }
    println!("{out}");
    Ok(())
}
