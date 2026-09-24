//! Evaluates flake development shells for rho through the Nix C API.
//!
//! Evaluation is pure, and the Nix fork reports every read of the flake's
//! sources with what it observed, so a shell can be cached against exactly
//! what it read (see `rho-devshell`). It links libnix, so rho runs it as a
//! separate process rather than loading Nix into its own.
//!
//!     rho-devshell-builder shell <flake-dir> [--shell NAME] [--roots DIR]
//!     rho-devshell-builder pin <store-path> <gc-root>
//!
//! `shell` evaluates and builds the shell and prints it, with what its
//! evaluation read, as JSON (`rho_devshell::Evaluated`). With `--roots`, a
//! shell that can be cached is pinned by a GC root in `DIR` before the
//! process, and with it Nix's temporary root, goes away. The agent base's
//! patched `nix develop` runs `shell` too.
//!
//! `pin` roots an existing store path, exiting with
//! `rho_devshell::PIN_GONE` if it was already garbage collected.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use devenv_eval_cache::{Checkout, Input, RevInputDesc, record_inputs};
use devenv_nix_backend::{DevShellRequest, NIX_STACK_SIZE, NixRuntime};
use rho_devshell::{Evaluated, Flake};

const USAGE: &str = "usage: rho-devshell-builder shell <flake-dir> [--shell NAME] [--roots DIR]
       rho-devshell-builder pin <store-path> <gc-root>";

enum Mode {
    Shell { flake: Flake, roots: Option<PathBuf> },
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
            let dir = std::fs::canonicalize(&dir).with_context(|| dir.clone())?;
            let mut shell = "default".to_owned();
            let mut roots = None;
            while let Ok(arg) = next("") {
                match arg.as_str() {
                    "--shell" => shell = next("--shell value")?,
                    "--roots" => roots = Some(next("--roots value")?.into()),
                    other => bail!("unknown argument {other}\n{USAGE}"),
                }
            }
            Ok(Mode::Shell {
                flake: Flake::new(dir, shell),
                roots,
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
        .without_time()
        .init();
    // Evaluation recurses deeply; match the Nix CLI's stack.
    std::thread::Builder::new()
        .stack_size(NIX_STACK_SIZE)
        .spawn(move || match mode {
            Mode::Shell { flake, roots } => {
                println!("{}", serde_json::to_string(&shell(&flake, roots.as_deref())?)?);
                Ok(())
            }
            Mode::Pin { store_path, gc_root } => {
                let mut nix = NixRuntime::new().map_err(|e| anyhow::anyhow!("{e:?}"))?;
                if let Some(dir) = gc_root.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                if !nix.pin(&gc_root, &store_path).map_err(|e| anyhow::anyhow!("{e:?}"))? {
                    std::process::exit(rho_devshell::PIN_GONE);
                }
                Ok(())
            }
        })?
        .join()
        .expect("evaluator thread panicked")
}

/// Evaluate and build `flake`'s shell, recording what evaluation read; with
/// `roots`, pin a shell that can be cached.
fn shell(flake: &Flake, roots: Option<&Path>) -> Result<Evaluated> {
    let source = &flake.source;
    let mut nix = NixRuntime::new().map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let request = DevShellRequest {
        flake_dir: flake.dir.clone(),
        system: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        shell: flake.shell.clone(),
    };
    let eval = nix
        .eval_dev_shell(&request)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let inputs = match record_inputs(&eval.ops, &source.root) {
        Ok((fetched_as, _)) if fetched_as != source.scheme => {
            bail!("flake was fetched as {fetched_as:?}, expected {:?}", source.scheme)
        }
        Ok((_, recorded)) => {
            let flake_rev = recorded.flake_rev;
            let mut inputs = recorded.into_inputs();
            if flake_rev {
                let checkout = Checkout::new(&source.root, source.scheme)?;
                inputs.push(Input::FlakeRev(RevInputDesc::new(&checkout)?));
            }
            Some(inputs)
        }
        Err(e) => {
            eprintln!("not caching: {e}");
            None
        }
    };
    let env_store_path = eval.shell.env_store_path;
    if let (Some(roots), Some(_)) = (roots, &inputs) {
        std::fs::create_dir_all(roots)?;
        let gc_root = rho_devshell::gc_root(roots, &env_store_path);
        nix.add_gc_root(&gc_root, &env_store_path)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(Evaluated {
        drv_path: eval.shell.drv_path,
        env_store_path,
        inputs,
    })
}
