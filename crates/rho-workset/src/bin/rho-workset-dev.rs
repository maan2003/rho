use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use rho_workset::{ExposedBuilder, FsViewBuilder, FsViewConfig, Mounts};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    let mut src = None;
    let mut stores = None;
    let mut socket = None;
    let mut skeleton = None;
    let mut exposed = false;
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--" {
            command.extend(args);
            break;
        }
        let value = match arg.to_str() {
            Some("--src" | "--stores" | "--socket" | "--skeleton") => args
                .next()
                .with_context(|| format!("missing value for {}", arg.to_string_lossy()))?,
            Some("--exposed") => {
                exposed = true;
                continue;
            }
            Some("-h" | "--help") => {
                usage();
                return Ok(());
            }
            _ => bail!("unknown argument: {}", arg.to_string_lossy()),
        };
        match arg.to_str().unwrap() {
            "--src" => src = Some(PathBuf::from(value)),
            "--stores" => stores = Some(PathBuf::from(value)),
            "--socket" => socket = Some(PathBuf::from(value)),
            "--skeleton" => skeleton = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
    }
    let src = std::path::absolute(src.context("--src is required")?)?;
    let store_root = std::path::absolute(stores.context("--stores is required")?)?;
    let store_socket = socket.map(std::path::absolute).transpose()?;
    let set = Mounts {
        src,
        store_root,
        store_socket,
    };
    let mut command = if command.is_empty() {
        vec![
            std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("bash")),
            OsString::from("-l"),
        ]
    } else {
        command
    };
    let status = if exposed {
        anyhow::ensure!(
            skeleton.is_none(),
            "--skeleton makes no sense in exposed mode: the host home is used as-is"
        );
        let builder = ExposedBuilder::new(set)?;
        // SAFETY: this dev program creates no threads before entering the builder.
        unsafe { builder.run(&command[0], &command[1..]) }?
    } else {
        command[0] = resolve_program(&command[0])?.into_os_string();
        let mut config = FsViewConfig::new(set)?;
        config.home_skeleton = skeleton;
        let builder = FsViewBuilder::new(config)?;
        // SAFETY: this dev program creates no threads before entering the builder.
        unsafe { builder.run(&command[0], &command[1..]) }?
    };
    std::process::exit(status.code().unwrap_or(128));
}

fn usage() {
    eprintln!(
        "usage: rho-workset-dev [--exposed] --src PATH --stores PATH [--socket PATH] [--skeleton PATH] [-- COMMAND ...]"
    );
}

fn resolve_program(program: &std::ffi::OsStr) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(program);
    let candidate = if path.components().count() > 1 {
        path
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(&path))
            .find(|candidate| candidate.is_file())
            .with_context(|| format!("command not found: {}", program.to_string_lossy()))?
    };
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("resolve command {}", candidate.display()))?;
    anyhow::ensure!(
        resolved.starts_with("/nix/store"),
        "command resolves outside /nix/store and will not exist in the view: {}",
        resolved.display()
    );
    Ok(resolved)
}
