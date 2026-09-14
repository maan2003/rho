//! Process-local runtime connections. Shared services remain in the daemon.

mod ipc;
mod process;
mod remote;
mod transport;
pub use process::Process;
mod workset;
pub use workset::{
    Action as WorksetAction, Attach as WorksetAttach, Client as WorksetClient,
    Reply as WorksetReply,
};
mod runtime;
mod services;
pub(crate) use ipc::{Host, StoreError};
pub use remote::Remote;

#[cfg(test)]
pub(crate) fn local_services(
    db: rho_db::RhoDb,
    inference: rho_inference::Inference,
    agent: rho_core::AgentId,
    pool: std::sync::Weak<crate::pool::AgentPool>,
) -> std::sync::Arc<Host> {
    let (client, server) = testing::pair();
    let services = std::sync::Arc::new(services::Services::new(
        db,
        inference,
        agent,
        pool,
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
    ));
    tokio::spawn(async move {
        let _ = services
            .serve(server.sender, server.port, server.incoming)
            .await;
    });
    client.host()
}

/// Entry point of the companion process; callers must not have started threads.
pub fn worker_main() -> anyhow::Result<()> {
    use std::io::Read as _;
    let mut socket = runtime::control_socket()?;
    let mut length = [0; 4];
    socket.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(length <= 1024 * 1024, "oversized workset startup");
    let mut bytes = vec![0; length];
    socket.read_exact(&mut bytes)?;
    let mut startup: process::Startup = senax_encoder::decode(&mut bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid workset startup"))?;
    anyhow::ensure!(startup.version == ipc::VERSION, "workset protocol mismatch");
    let mut config_home = startup.claude.config_home().to_owned();
    if matches!(startup.layout.mode, rho_fs_view::Mode::View { .. }) {
        if let Ok(home) = std::env::var("HOME")
            && let Ok(relative) = config_home.strip_prefix(&home)
        {
            config_home = camino::Utf8Path::new(rho_fs_view::AGENT_HOME).join(relative);
        }
    }
    unsafe {
        startup.layout.build()?;
    }
    startup.claude = rho_claude::namespace::install_sources(
        &startup.claude,
        startup.layout.staging_root(),
        startup.layout.state.as_std_path(),
        config_home,
    )?;
    let view = unsafe { startup.layout.enter()? };
    // This process owns provider transports, but does not start the daemon's
    // RPC listener (which installs its own TLS provider).
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("workset TLS provider already initialized"))?;
    socket.set_nonblocking(true)?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async {
            runtime::run(tokio::net::UnixStream::from_std(socket)?, startup, view).await
        })
}

#[cfg(test)]
pub(super) mod testing {
    use super::*;
    pub struct Endpoint {
        pub(super) sender: transport::Sender,
        pub(super) port: transport::Port,
        pub(super) incoming: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    }
    impl Endpoint {
        pub fn host(self) -> std::sync::Arc<Host> {
            Host::connect(
                self.sender,
                self.port,
                self.incoming,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            )
        }
        pub(super) async fn read(&mut self) -> std::io::Result<ipc::Message<'static>> {
            ipc::decode(
                &self
                    .incoming
                    .recv()
                    .await
                    .ok_or(std::io::ErrorKind::UnexpectedEof)?,
            )
        }
        pub(super) async fn write(&self, message: &ipc::Message<'_>) -> std::io::Result<()> {
            self.sender.send(self.port, ipc::encode(message)?).await
        }
    }
    pub fn pair() -> (Endpoint, Endpoint) {
        let (left, right) = tokio::net::UnixStream::pair().unwrap();
        fn endpoint(socket: tokio::net::UnixStream) -> Endpoint {
            let (sender, mut receiver, writer) = transport::connect(socket);
            let (incoming, messages) = tokio::sync::mpsc::channel(32);
            tokio::spawn(async move {
                tokio::select! {
                    _ = incoming.closed() => {}
                    _ = async {
                        while let Ok(packet) = receiver.next().await {
                            if incoming.send(packet.bytes).await.is_err() { break; }
                        }
                    } => {}
                }
                writer.abort();
            });
            Endpoint {
                sender,
                port: transport::Port::Workset,
                incoming: messages,
            }
        }
        (endpoint(left), endpoint(right))
    }
}
