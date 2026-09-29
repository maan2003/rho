//! Worker-owned runtimes, host-service client, and workset execution.

mod claude;
pub(crate) mod host_client;
mod image_tool;
pub mod native;
mod runtime;
pub(crate) mod shared;
pub mod shell;
pub mod terminal;
mod workset;

use crate::ipc::protocol;

/// Entry point of the companion process; callers must not have started threads.
pub fn worker_main(factory: crate::inference::WorkerFactory) -> anyhow::Result<()> {
    use std::io::Read as _;
    let mut socket = runtime::control_socket()?;
    let requests = runtime::requests_socket()?;
    let mut length = [0; 4];
    socket.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(length <= 1024 * 1024, "oversized workset startup");
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes)?;
    let mut startup: protocol::Startup = senax_encoder::decode(&mut bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid workset startup"))?;
    anyhow::ensure!(
        startup.version == protocol::VERSION,
        "workset protocol mismatch"
    );
    let config_home = startup.claude.config_home().to_owned();
    unsafe {
        startup.layout.build()?;
    }
    startup.claude = rho_claude::namespace::install_sources(
        &startup.claude,
        std::path::Path::new("/"),
        startup.layout.state.as_std_path(),
        config_home,
    )?;
    unsafe { startup.layout.enter()? };
    // This process owns provider transports, but does not start the agent host's
    // RPC listener (which installs its own TLS provider).
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("workset TLS provider already initialized"))?;
    socket.set_nonblocking(true)?;
    requests.set_nonblocking(true)?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async {
            runtime::run(
                tokio::net::UnixStream::from_std(socket)?,
                tokio::net::UnixStream::from_std(requests)?,
                startup,
                factory,
            )
            .await
        })
}
