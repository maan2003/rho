//! Heap profiles on demand. The agent host and workset binaries have jemalloc
//! sample allocations from startup; `SIGUSR1` writes what is live to a file.
use std::os::unix::ffi::OsStrExt as _;

/// The `malloc_conf` both binaries export: sampling on from the first
/// allocation, at jemalloc's default of one sample per 512 KiB.
pub const MALLOC_CONF: &[u8; 27] = b"prof:true,prof_active:true\0";

/// Writes a heap profile to the temporary directory on every `SIGUSR1`.
pub async fn dump_on_sigusr1() {
    let mut signals =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()) {
            Ok(signals) => signals,
            Err(error) => return eprintln!("rho-agent: no heap profiles: {error}"),
        };
    while signals.recv().await.is_some() {
        let path = std::env::temp_dir().join(format!(
            "rho-heap-{}-{}.heap",
            std::process::id(),
            rho_agent_types::UnixMs::now().0
        ));
        match dump(&path) {
            Ok(()) => eprintln!("rho-agent: wrote heap profile to {}", path.display()),
            Err(error) => eprintln!("rho-agent: heap profile failed: {error:#}"),
        }
    }
}

fn dump(path: &std::path::Path) -> anyhow::Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", path.as_ptr()) }
        .map_err(|error| anyhow::anyhow!("prof.dump: {error}"))
}
