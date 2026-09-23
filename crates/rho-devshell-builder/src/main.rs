//! Builds flake development shells for rho through the Nix C API, and caches
//! the built environment against exactly what evaluation read.
//!
//! Evaluation is pure, and the Nix fork reports every read of the flake's
//! sources with what it observed, so a cached shell stays valid while those
//! observations still hold. It links libnix, so rho runs it as a separate
//! process rather than loading Nix into its own.
//!
//!     rho-devshell-builder shell <flake-dir> [--shell NAME] [--cache DB] [--no-cache]
//!     rho-devshell-builder exec [--] PROGRAM [ARGS...]
//!     rho-devshell-builder develop NIX [ARGS...]
//!
//! `shell` prints the shell as JSON, for the agent worker to activate and
//! watch. `exec` runs a program in the dev shell of the nearest flake above
//! the working directory, or as it is outside flakes. `develop` is
//! `nix develop ARGS...` from the cache for a local flake's dev shell, with
//! or without `--command`, and hands anything else to the real `NIX`.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use devenv_core::ObservedKind;
use devenv_eval_cache::{
    CachingEvalService, Checkout, EvalCacheKey, FlakeScheme, Input, RevInputDesc, record_inputs,
};
use devenv_nix_backend::build_environment::BuildEnvironment;
use devenv_nix_backend::{DevShellRequest, NIX_STACK_SIZE, NixRuntime};

/// Changes whenever evaluation semantics change (evaluator, Nix fork patches).
const EVALUATOR: &str = concat!("rho-devshell-builder/", env!("CARGO_PKG_VERSION"), " nix-2.35-rho2");

const USAGE: &str = "usage: rho-devshell-builder shell <flake-dir> [--shell NAME] [--cache DB] [--no-cache]
       rho-devshell-builder exec [--] PROGRAM [ARGS...]
       rho-devshell-builder develop NIX [ARGS...]";

/// Bash running a program after the shell's activation script, which it
/// reads from the descriptor in `$1`. `shellHook` output goes to stderr so
/// that stdout is the program's alone.
const EXEC_SCRIPT: &str = r#"fd=$1; shift; . "/dev/fd/$fd" >&2; exec {fd}<&-; unset fd; exec "$@""#;

enum Mode {
    Shell(Args),
    Exec(Vec<OsString>),
    Develop { nix: OsString, args: Vec<OsString> },
}

struct Args {
    flake_dir: PathBuf,
    shell: String,
    cache: Option<PathBuf>,
}

impl Args {
    fn new(flake_dir: PathBuf) -> Self {
        Self {
            flake_dir,
            shell: "default".into(),
            cache: Some(default_cache_path()),
        }
    }
}

fn parse_args() -> Result<Mode> {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_ref().and_then(|mode| mode.to_str()) {
        Some("shell") => {}
        Some("exec") => {
            let mut program: Vec<OsString> = args.collect();
            if program.first().is_some_and(|arg| arg == "--") {
                program.remove(0);
            }
            if program.is_empty() {
                bail!("{USAGE}");
            }
            return Ok(Mode::Exec(program));
        }
        Some("develop") => {
            let nix = args.next().context(USAGE)?;
            return Ok(Mode::Develop { nix, args: args.collect() });
        }
        _ => bail!("{USAGE}"),
    }
    let mut args = args.map(|arg| arg.into_string().map_err(|arg| anyhow::anyhow!("non-UTF-8 argument {arg:?}")));
    let flake_dir = args.next().context("missing flake directory")??;
    let flake_dir = std::fs::canonicalize(&flake_dir).with_context(|| flake_dir.clone())?;
    let mut parsed = Args::new(flake_dir);
    while let Some(arg) = args.next() {
        match arg?.as_str() {
            "--shell" => parsed.shell = args.next().context("--shell needs a value")??,
            "--cache" => parsed.cache = Some(args.next().context("--cache needs a value")??.into()),
            "--no-cache" => parsed.cache = None,
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(Mode::Shell(parsed))
}

fn default_cache_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RHO_DEVSHELL_CACHE") {
        return path.into();
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    base.join("rho/devshell-cache.sqlite")
}

fn main() -> Result<()> {
    let mode = parse_args()?;
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
        .spawn(move || match mode {
            Mode::Shell(args) => report(&build(&args)?),
            Mode::Exec(program) => exec(&program),
            Mode::Develop { nix, args } => develop(&nix, &args),
        })?
        .join()
        .expect("evaluator thread panicked")
}

/// Run `program` in the dev shell of the nearest flake above the working
/// directory. A shell that fails to build is reported and the program runs
/// without it, as it would outside the flake.
fn exec(program: &[OsString]) -> Result<()> {
    let cwd = std::env::current_dir().context("working directory")?;
    let activation = match find_flake(&cwd) {
        None => None,
        Some(flake_dir) => match build(&Args::new(flake_dir)).and_then(|built| activation(&built.shell)) {
            Ok(activation) => Some(activation),
            Err(e) => {
                eprintln!("rho: dev shell unavailable, running without it: {e:#}");
                None
            }
        },
    };
    match activation {
        Some(activation) => run_in_shell(&activation, program),
        None => Err(std::process::Command::new(&program[0]).args(&program[1..]).exec())
            .with_context(|| format!("exec {:?}", program[0])),
    }
}

/// Exec Bash running `program` after `activation`.
fn run_in_shell(activation: &str, program: &[OsString]) -> Result<()> {
    let fd = script_fd(activation)?;
    Err(std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-c", EXEC_SCRIPT, "bash"])
        .arg(std::os::fd::AsRawFd::as_raw_fd(&fd).to_string())
        .args(program)
        .exec())
    .context("exec bash")
}

/// `script` in an inheritable memfd: an activation script is too large for
/// an argument.
fn script_fd(script: &str) -> Result<OwnedFd> {
    let fd = unsafe { libc::memfd_create(c"rho-devshell-activation".as_ptr(), 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    std::fs::File::from(fd.try_clone()?).write_all(script.as_bytes())?;
    Ok(fd)
}

/// `nix develop ARGS...` for the cases the cache answers: an optional local
/// flake (`.`, `./dir`, `/dir`, with `#NAME` for another dev shell) and an
/// optional `--command`/`-c`. Any other form, and any shell that fails to
/// build here, goes to the real Nix, which also reports errors as Nix does.
fn develop(nix: &OsString, args: &[OsString]) -> Result<()> {
    let real_nix = || {
        Err(std::process::Command::new(nix).arg("develop").args(args).exec())
            .with_context(|| format!("exec {nix:?}"))
    };
    let Some((installable, command)) = parse_develop(args) else {
        return real_nix();
    };
    let (path, shell) = match installable.split_once('#') {
        Some((path, shell)) => (path, shell),
        None => (installable, ""),
    };
    let Some(flake_dir) = find_flake(Path::new(if path.is_empty() { "." } else { path })) else {
        return real_nix();
    };
    let mut build_args = Args::new(flake_dir);
    if !shell.is_empty() {
        build_args.shell = shell.to_owned();
    }
    let Ok(activation) = build(&build_args).and_then(|built| activation(&built.shell)) else {
        return real_nix();
    };
    match command {
        Some(command) => run_in_shell(&activation, command),
        None => {
            // Interactive, as `nix develop` starts it: the shell is the rc file.
            let fd = script_fd(&activation)?;
            let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
            Err(std::process::Command::new("bash")
                .arg("--rcfile")
                .arg(format!("/dev/fd/{raw}"))
                .exec())
            .context("exec bash")
        }
    }
}

/// The installable (`.` when absent) and command of a `nix develop`
/// invocation the cache can answer.
fn parse_develop(args: &[OsString]) -> Option<(&str, Option<&[OsString]>)> {
    let mut installable = None;
    let mut command = None;
    for (i, arg) in args.iter().enumerate() {
        let arg = arg.to_str()?;
        if matches!(arg, "-c" | "--command") {
            command = Some(&args[i + 1..]).filter(|command| !command.is_empty());
            command?;
            break;
        }
        if arg.starts_with('-') || installable.is_some() {
            return None;
        }
        installable = Some(arg);
    }
    let installable = installable.unwrap_or(".");
    let (path, shell) = installable.split_once('#').unwrap_or((installable, ""));
    // A bare word is a registry flake; a dotted fragment is an attribute path.
    let local = path.is_empty() || path.starts_with('.') || path.starts_with('/');
    (local && !path.contains(':') && !shell.contains('.')).then_some((installable, command))
}

/// The nearest directory with a `flake.nix`, looking no further up than the
/// enclosing git checkout.
fn find_flake(cwd: &Path) -> Option<PathBuf> {
    let physical = cwd.canonicalize().ok()?;
    for dir in physical.ancestors() {
        if dir.join("flake.nix").is_file() {
            return Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
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

/// A shell and how it was obtained.
struct Built {
    outcome: &'static str,
    shell: Shell,
    /// The cache candidate, if the shell could be cached.
    eval_id: Option<i64>,
    watch: Watch,
    started: Instant,
    /// Lookup and evaluation time, inputs recorded and effects seen.
    eval: Option<(Duration, Duration, usize, usize)>,
}

fn build(args: &Args) -> Result<Built> {
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
                return Ok(Built {
                    outcome: "hit",
                    shell,
                    eval_id: Some(eval_id),
                    watch: watch_paths(&source, &args.flake_dir, &inputs),
                    started: t0,
                    eval: None,
                });
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
            return Ok(Built {
                outcome: "uncacheable",
                shell,
                eval_id: None,
                watch: Watch::default(),
                started: t0,
                eval: Some((lookup_done, eval_done, 0, eval.ops.len())),
            });
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
    Ok(Built {
        outcome: "miss",
        shell,
        eval_id,
        watch: watch_paths(&source, &args.flake_dir, &inputs),
        started: t0,
        eval: stats,
    })
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

/// Bash applying `shell` to the caller's environment, `shellHook` included.
/// `RHO_DEVSHELL_PATH_PREFIX` then goes before the shell's `PATH`.
fn activation(shell: &Shell) -> Result<String> {
    let mut script = BuildEnvironment::from_json(&shell.env_json)?.to_activation_script();
    script.push_str(
        "\nif [ -n \"${RHO_DEVSHELL_PATH_PREFIX-}\" ]; then PATH=\"$RHO_DEVSHELL_PATH_PREFIX:$PATH\"; export PATH; fi\n",
    );
    Ok(script)
}

/// Print the result for the caller: the shell as a Bash activation script,
/// the candidate it is cached as, and what to watch to know it may be stale.
fn report(built: &Built) -> Result<()> {
    let mut out = serde_json::json!({
        "outcome": built.outcome,
        "eval_id": built.eval_id,
        "drv_path": built.shell.drv_path,
        "env_store_path": built.shell.env_store_path,
        "activation": activation(&built.shell)?,
        "watch": built.watch.contents,
        "watch_names": built.watch.names,
        "total_s": built.started.elapsed().as_secs_f64(),
    });
    if let Some((lookup, eval, inputs, ops)) = built.eval {
        out["lookup_s"] = lookup.as_secs_f64().into();
        out["eval_s"] = (eval - lookup).as_secs_f64().into();
        out["inputs"] = inputs.into();
        out["effects"] = ops.into();
    }
    println!("{out}");
    Ok(())
}
