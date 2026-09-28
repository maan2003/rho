//! A minimal process owner for the socket and CRIU integration tests.
use std::os::fd::FromRawFd;
use std::sync::Arc;
use std::time::Duration;

use rho_notebook::{Notebook, process};
use rho_tool_shell::ShellTools;
use tokio::sync::Notify;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workdir = std::env::args().nth(1).ok_or("expected workdir")?;
    let shell =
        ShellTools::in_directory(Duration::from_secs(20), workdir.into(), Default::default());
    let wake = Arc::new(Notify::new());
    let notebook = Notebook::new(shell, Vec::new(), Arc::clone(&wake))?;
    // The test launcher installs its socket at fd 3 before exec.
    let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    process::serve(notebook, socket, wake).await?;
    Ok(())
}
