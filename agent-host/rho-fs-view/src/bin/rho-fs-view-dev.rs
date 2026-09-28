//! Development runner: enters a workset namespace the way the agent host does
//! and runs one command in it. `--src` is adopted as the workset; `--state`
//! is a state root (its `stores/` is the mirror store). With `--store` the
//! keeper runs and `git` inside is Rho's patched git, as under the
//! agent host; without it no keeper runs and `git` inside is the plain one.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use rho_fs_view::{PathOverrides, StoreRefresh, StoreService, UserEnvironment, Worksets};

fn main() -> anyhow::Result<()> {
    let incoming = std::env::args_os().collect::<Vec<_>>();
    if incoming.get(1).is_some_and(|arg| arg == "--enter-layout") {
        let bytes = std::fs::read(&incoming[2])?;
        let layout: rho_fs_view::WorksetLayout = senax_encoder::decode(&mut bytes.as_slice())
            .map_err(|_| anyhow::anyhow!("invalid workset layout"))?;
        unsafe {
            layout.build()?;
            layout.enter()?;
        }
        return Err(std::process::Command::new(&incoming[3])
            .args(&incoming[4..])
            .exec()
            .into());
    }
    let mut args = std::env::args_os().skip(1).peekable();
    let mut src = None;
    let mut state = None;
    let mut store = false;
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--" {
            command.extend(args);
            break;
        }
        let value = match arg.to_str() {
            Some("--src" | "--state") => args
                .next()
                .with_context(|| format!("missing value for {}", arg.to_string_lossy()))?,
            Some("--store") => {
                store = true;
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
            "--state" => state = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
    }
    let src = std::path::absolute(src.context("--src is required")?)?;
    let state = std::path::absolute(state.context("--state is required")?)?;
    let command = if command.is_empty() {
        vec![
            std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("bash")),
            OsString::from("-l"),
        ]
    } else {
        command
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let status = runtime.block_on(async {
        let worksets = Worksets::open(
            &state,
            UserEnvironment::new(std::env::vars_os().collect()),
            PathOverrides::default(),
            if store {
                StoreService::Serve(StoreRefresh::default())
            } else {
                StoreService::None
            },
        )
        .await?;
        let workset = worksets.adopt(&src)?;
        let layout = rho_fs_view::WorksetLayout::new(&workset)?;
        let mut description = tempfile::NamedTempFile::new()?;
        std::io::Write::write_all(
            &mut description,
            &senax_encoder::encode(&layout).map_err(|_| anyhow::anyhow!("encode layout"))?,
        )?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?);
        child
            .arg("--enter-layout")
            .arg(description.path())
            .args(&command);
        child
            .status()
            .await
            .with_context(|| format!("run {}", command[0].to_string_lossy()))
    })?;
    std::process::exit(status.code().unwrap_or(128));
}

fn usage() {
    eprintln!("usage: rho-fs-view-dev [--store] --src PATH --state PATH [-- COMMAND ...]");
}
