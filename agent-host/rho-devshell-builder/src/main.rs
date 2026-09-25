//! Resolves flake development shells for rho through the Nix C API.
//!
//! Evaluation is pure, and the Nix fork records what it observed of local
//! inputs, so a shell can be cached against exactly that and checked by
//! observing the same things again (see `rho-devshell`). It links libnix,
//! so rho runs it as a separate process rather than loading Nix into its
//! own.
//!
//!     rho-devshell-builder shell <flake-dir> [--shell NAME] [--dir DIR] [--no-cache]
//!     rho-devshell-builder activate <env-store-path> --dir DIR
//!     rho-devshell-builder pin <store-path> <gc-root>
//!
//! `shell` finds the shell in the daemon's cache in `DIR` (by default
//! `$RHO_DEVSHELL_DIR`) unless `--no-cache`, pinning it, or evaluates and
//! caches it; writes its activation script into `DIR`; and prints it as
//! JSON (`rho_devshell::Shell`). The agent base's patched `nix develop`
//! runs it too, and reads `env_store_path`. A flake without a shell exits
//! with `rho_devshell::SHELL_FAILED`, the error on stderr and what to watch
//! (`rho_devshell::Watch`) as JSON.
//!
//! `activate` writes an environment's activation script into `DIR`.
//!
//! `pin` roots an existing store path, exiting with
//! `rho_devshell::PIN_GONE` if it was already garbage collected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use devenv_nix_backend::logger::strip_ansi;
use devenv_nix_backend::{DevShellRequest, LocalInput, NIX_STACK_SIZE, NixRuntime};
use rho_devshell::{Client, Evaluated, Flake, InputUrl, Observation, SHELL_FAILED, Shell, Watch};

const USAGE: &str = "usage: rho-devshell-builder shell <flake-dir> [--shell NAME] [--dir DIR] [--no-cache]
       rho-devshell-builder activate <env-store-path> --dir DIR
       rho-devshell-builder pin <store-path> <gc-root>";

enum Mode {
    Shell { flake: Flake, dir: Option<PathBuf>, cache: bool },
    Activate { env_store_path: String, dir: PathBuf },
    Pin { store_path: String, gc_root: PathBuf },
}

fn parse_args() -> Result<Mode> {
    let mut args = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string().map_err(|arg| anyhow::anyhow!("non-UTF-8 argument {arg:?}")));
    let mut next = |what: &str| -> Result<String> { args.next().with_context(|| format!("missing {what}\n{USAGE}"))? };
    match next("mode")?.as_str() {
        "shell" => {
            let dir = next("flake directory")?;
            let flake_dir = std::fs::canonicalize(&dir).with_context(|| dir.clone())?;
            let mut shell = "default".to_owned();
            let mut dir = std::env::var_os("RHO_DEVSHELL_DIR").map(PathBuf::from);
            let mut cache = true;
            while let Ok(arg) = next("") {
                match arg.as_str() {
                    "--shell" => shell = next("--shell value")?,
                    "--dir" => dir = Some(next("--dir value")?.into()),
                    "--no-cache" => cache = false,
                    other => bail!("unknown argument {other}\n{USAGE}"),
                }
            }
            Ok(Mode::Shell {
                flake: Flake::new(flake_dir, shell),
                dir,
                cache,
            })
        }
        "activate" => {
            let env_store_path = next("environment store path")?;
            if next("--dir")? != "--dir" {
                bail!("{USAGE}");
            }
            Ok(Mode::Activate {
                env_store_path,
                dir: next("--dir value")?.into(),
            })
        }
        "pin" => Ok(Mode::Pin {
            store_path: next("store path")?,
            gc_root: next("GC root")?.into(),
        }),
        _ => bail!("{USAGE}"),
    }
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
        // Nix's messages carry their own `warning:` or `error:`.
        .with_level(false)
        .without_time()
        .init();
    // Evaluation recurses deeply; match the Nix CLI's stack.
    std::thread::Builder::new()
        .stack_size(NIX_STACK_SIZE)
        .spawn(move || {
            let mut nix = NixRuntime::new().map_err(|e| anyhow::anyhow!(plain(e.chain())))?;
            match mode {
                Mode::Shell { flake, dir, cache } => match shell(&mut nix, &flake, dir.as_deref(), cache)? {
                    Ok(shell) => println!("{}", serde_json::to_string(&shell)?),
                    Err(failure) => {
                        eprintln!("{}", failure.error);
                        println!("{}", serde_json::to_string(&failure.watch)?);
                        std::process::exit(SHELL_FAILED);
                    }
                },
                Mode::Activate { env_store_path, dir } => {
                    println!("{}", write_activation(&nix, &dir, &env_store_path)?.display());
                }
                Mode::Pin { store_path, gc_root } => {
                    if !pin(&mut nix, &gc_root, &store_path)? {
                        std::process::exit(rho_devshell::PIN_GONE);
                    }
                }
            }
            Ok(())
        })?
        .join()
        .expect("evaluator thread panicked")
}

/// `flake`'s shell: a valid cached one, pinned, or a new evaluation, cached
/// if it can be. Cache failures are reported and otherwise ignored.
fn shell(nix: &mut NixRuntime, flake: &Flake, dir: Option<&Path>, cache: bool) -> Result<Result<Shell, Failure>> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let client = dir.filter(|_| cache).map(Client::new);
    let mut key = flake.key()?;
    let mut found = None;
    if let (Some(client), Some(dir)) = (&client, dir) {
        match runtime.block_on(cached(nix, client, flake, &key, dir)) {
            Ok(hit) => found = hit,
            Err(e) => eprintln!("rho: dev shell cache unavailable: {e:#}"),
        }
    }
    let (id, evaluated) = match found {
        Some(hit) => hit,
        None => {
            let mut evaluation = match evaluate(nix, flake) {
                Ok(evaluation) => evaluation,
                Err(failure) => return Ok(Err(failure)),
            };
            // Locking that wrote `flake.lock` changed what evaluation had
            // observed of it: evaluate the locked flake again, to cache that.
            let mut stable = true;
            let locked = flake.key()?;
            if locked != key {
                evaluation = match evaluate(nix, flake) {
                    Ok(evaluation) => evaluation,
                    Err(failure) => return Ok(Err(failure)),
                };
                stable = flake.key()? == locked;
                key = locked;
            }
            let mut id = None;
            if let (Some(client), Some(dir), Some(evaluated), true) = (&client, dir, evaluation.cacheable(), stable) {
                match runtime.block_on(store(nix, client, &key, dir, &evaluated)) {
                    Ok(stored) => id = Some(stored),
                    Err(e) => eprintln!("rho: failed to cache dev shell: {e:#}"),
                }
            }
            (id, evaluation.into_evaluated())
        }
    };
    let activation = dir
        .map(|dir| write_activation(nix, dir, &evaluated.env_store_path))
        .transpose()?;
    Ok(Ok(Shell {
        id,
        watch: Watch::new(flake, &evaluated.observations),
        env_store_path: evaluated.env_store_path,
        activation,
    }))
}

/// A flake without a shell: why, and what to watch for that to change.
struct Failure {
    error: String,
    watch: Watch,
}

/// An error from Nix as plain text: each context, then what it wraps, a
/// line each.
fn plain<'a>(chain: impl Iterator<Item = &'a (dyn std::error::Error + 'static)>) -> String {
    let lines: Vec<String> = chain.map(|error| error.to_string().trim_matches('\n').to_owned()).collect();
    strip_ansi(&lines.join("\n"))
}


/// The newest cached shell whose observations all hold now and whose
/// environment could be pinned.
async fn cached(
    nix: &mut NixRuntime,
    client: &Client,
    flake: &Flake,
    key: &str,
    dir: &Path,
) -> Result<Option<(Option<u64>, Evaluated)>> {
    let candidates = client.lookup(key.to_owned()).await?;
    let mut now = Now::new(&flake.source.root);
    for candidate in candidates {
        let Ok(evaluated) = serde_json::from_slice::<Evaluated>(&candidate.data) else {
            continue;
        };
        if !now.holds(nix, &evaluated.observations) {
            continue;
        }
        let gc_root = rho_devshell::gc_root(&rho_devshell::roots_dir(dir), &candidate.env_store_path);
        if !client.used(candidate.id).await? && !pin(nix, &gc_root, &candidate.env_store_path)? {
            client.forget(candidate.id).await?;
            continue;
        }
        return Ok(Some((Some(candidate.id), evaluated)));
    }
    Ok(None)
}

/// What local inputs observe now, each input opened and each thing
/// observed once: candidates mostly share observations.
struct Now<'a> {
    root: &'a Path,
    inputs: HashMap<String, Option<LocalInput>>,
    seen: HashMap<(String, String, String), Option<String>>,
}

impl<'a> Now<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            inputs: HashMap::new(),
            seen: HashMap::new(),
        }
    }

    fn holds(&mut self, nix: &NixRuntime, observations: &[Observation]) -> bool {
        observations
            .iter()
            .all(|observation| self.observe(nix, observation).as_ref() == Some(&observation.value))
    }

    fn observe(&mut self, nix: &NixRuntime, observation: &Observation) -> Option<String> {
        let url = observation.input.url(self.root);
        let seen = (url.clone(), observation.kind.clone(), observation.path.clone());
        if let Some(value) = self.seen.get(&seen) {
            return value.clone();
        }
        let value = self
            .inputs
            .entry(url)
            .or_insert_with_key(|url| nix.open_local_input(url).ok())
            .as_mut()
            .and_then(|input| input.observe(&observation.kind, &observation.path).ok());
        self.seen.insert(seen, value.clone());
        value
    }
}

struct Evaluation {
    drv_path: String,
    env_store_path: String,
    /// The observations, if the shell can be cached.
    cacheable: Option<Vec<Observation>>,
}

impl Evaluation {
    /// What to cache, if the shell can be cached.
    fn cacheable(&self) -> Option<Evaluated> {
        Some(Evaluated {
            drv_path: self.drv_path.clone(),
            env_store_path: self.env_store_path.clone(),
            observations: self.cacheable.clone()?,
        })
    }

    fn into_evaluated(self) -> Evaluated {
        Evaluated {
            drv_path: self.drv_path,
            env_store_path: self.env_store_path,
            observations: self.cacheable.unwrap_or_default(),
        }
    }
}

fn evaluate(nix: &mut NixRuntime, flake: &Flake) -> Result<Evaluation, Failure> {
    let request = DevShellRequest {
        flake_dir: flake.dir.clone(),
        system: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        shell: flake.shell.clone(),
    };
    let failed = |error: String, observed: &[devenv_nix_backend::Observation]| {
        let root = &flake.source.root;
        let observations: Vec<Observation> = observed
            .iter()
            .filter_map(|observation| {
                Some(Observation {
                    kind: observation.kind.clone(),
                    input: InputUrl::parse(&observation.input, root)?,
                    path: observation.path.clone(),
                    value: observation.value.clone(),
                })
            })
            .collect();
        Failure {
            error,
            watch: Watch::failed(flake, &observations),
        }
    };
    let eval = nix.eval_dev_shell(&request).map_err(|error| failed(plain(error.chain()), &[]))?;
    let shell = match eval.shell {
        Ok(shell) => shell,
        Err(error) => return Err(failed(plain(error.chain()), &eval.observations)),
    };
    let cacheable = match record(&flake.source.root, eval.observations) {
        Ok(observations) => Some(observations),
        Err(e) => {
            eprintln!("rho: not caching dev shell: {e}");
            None
        }
    };
    Ok(Evaluation {
        drv_path: shell.drv_path,
        env_store_path: shell.env_store_path,
        cacheable,
    })
}

/// Evaluation's observations as every checkout of the flake at `root` can
/// check them, if they can be: the flake's source was observed, and
/// nothing changed while evaluation read it.
fn record(root: &Path, observed: Vec<devenv_nix_backend::Observation>) -> Result<Vec<Observation>, String> {
    let mut values: HashMap<(String, InputUrl, String), String> = HashMap::new();
    let mut observations = Vec::new();
    for observation in observed {
        let input = InputUrl::parse(&observation.input, root)
            .ok_or_else(|| format!("cannot name the local input {}", observation.input))?;
        let identity = (observation.kind.clone(), input.clone(), observation.path.clone());
        if let Some(previous) = values.insert(identity, observation.value.clone())
            && previous != observation.value
        {
            return Err(format!(
                "{} changed while it was being evaluated",
                input.dir(root).join(&observation.path).display()
            ));
        }
        observations.push(Observation {
            kind: observation.kind,
            input,
            path: observation.path,
            value: observation.value,
        });
    }
    if !observations.iter().any(|o| o.input.dir.as_os_str().is_empty()) {
        return Err(format!("evaluation observed nothing of {}", root.display()));
    }
    observations.sort();
    observations.dedup();
    Ok(observations)
}

/// Pin a new cacheable shell, before this process and with it Nix's
/// temporary root go away, then store it; returns its entry.
async fn store(nix: &mut NixRuntime, client: &Client, key: &str, dir: &Path, evaluated: &Evaluated) -> Result<u64> {
    let env_store_path = &evaluated.env_store_path;
    let roots = rho_devshell::roots_dir(dir);
    std::fs::create_dir_all(&roots)?;
    nix.add_gc_root(&rho_devshell::gc_root(&roots, env_store_path), env_store_path)
        .map_err(|e| anyhow::anyhow!(plain(e.chain())))?;
    client
        .store(key.to_owned(), env_store_path.clone(), serde_json::to_vec(evaluated)?)
        .await
}

/// Root `store_path` at `gc_root` if it is still valid; whether it was.
fn pin(nix: &mut NixRuntime, gc_root: &Path, store_path: &str) -> Result<bool> {
    if let Some(dir) = gc_root.parent() {
        std::fs::create_dir_all(dir)?;
    }
    nix.pin(gc_root, store_path).map_err(|e| anyhow::anyhow!(plain(e.chain())))
}

/// Write `env_store_path`'s activation script into the cache directory
/// `dir` unless it is there; its path.
fn write_activation(nix: &NixRuntime, dir: &Path, env_store_path: &str) -> Result<PathBuf> {
    let path = rho_devshell::activation_path(dir, env_store_path);
    if path.exists() {
        return Ok(path);
    }
    let json = std::fs::read_to_string(env_store_path).with_context(|| format!("read {env_store_path}"))?;
    let data = path.with_extension("d");
    std::fs::create_dir_all(&data)?;
    let script = nix
        .rc_script(&json, &data, &data.join("outputs"))
        .map_err(|e| anyhow::anyhow!(plain(e.chain())))?;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    std::io::Write::write_all(&mut temporary, script.as_bytes())?;
    temporary.persist(&path)?;
    Ok(path)
}
