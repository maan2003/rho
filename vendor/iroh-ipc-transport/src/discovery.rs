//! Filesystem rendezvous, with leases instead of socket liveness probes.

use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iroh::{EndpointAddr, EndpointId, TransportAddr};
use iroh_base::CustomAddr;
use tokio::io::AsyncReadExt;

use crate::IPC_TRANSPORT_ID;
use crate::directory_access::DirectoryAccess;
use crate::locking::Lock;

/// Local rendezvous directory shared by cooperating processes.
///
/// Records are useful only while their owner's advisory lease is held. No
/// networking, DNS, PID inspection, or Unix-socket connect is used to discover
/// a peer. The directory must be on a local filesystem supporting file locks.
/// QUIC authenticates peers; directory access alone grants no peer identity.
#[derive(Debug, Clone)]
pub struct LocalDiscovery {
    root: PathBuf,
    access: DirectoryAccess,
}

impl LocalDiscovery {
    /// Choose a rendezvous directory private to the current user. If missing,
    /// it is created on first use with mode `0700`, independent of the umask.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_access(root, DirectoryAccess::Owner)
    }

    /// Choose a rendezvous directory shared by every local user. If missing,
    /// it is created on first use with mode `01777` (world-writable, sticky),
    /// independent of the umask, so peers running as different users can
    /// rendezvous. An existing directory keeps whatever mode it has.
    pub fn shared(root: impl Into<PathBuf>) -> Self {
        Self::with_access(root, DirectoryAccess::Shared)
    }

    fn with_access(root: impl Into<PathBuf>, access: DirectoryAccess) -> Self {
        Self {
            root: root.into(),
            access,
        }
    }

    /// The rendezvous directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Select a local-only dial target for a live co-located peer.
    /// Targets without a local lease retain their supplied addresses.
    ///
    /// Call before connecting even when a target already contains IP or relay
    /// addresses. iroh 1.3 races initial packets and may leave a losing custom
    /// path unopened. A path selector can only prefer paths that were opened.
    /// A local lease proves co-location, so remote candidates are unnecessary
    /// for this dial. Transport errors are returned without a remote retry.
    pub async fn resolve_target(&self, target: EndpointAddr) -> io::Result<EndpointAddr> {
        match self.resolve(target.id).await? {
            Some(local) => Ok(EndpointAddr::from_parts(target.id, [local])),
            None => Ok(target),
        }
    }

    /// Resolve an actively leased local endpoint, or `None` if absent/stale.
    /// Malformed records and filesystem failures are returned as errors.
    pub async fn resolve(&self, id: EndpointId) -> io::Result<Option<TransportAddr>> {
        if !tokio::fs::try_exists(&self.root).await? {
            return Ok(None);
        }
        let _guard = self.guard().await?;
        let path = self.record_path(id);
        match Lock::open(&self.lease_path(id), false).await {
            Ok(_) => return Ok(None), // The previous process released its lease.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        let mut file = tokio::fs::File::open(path).await?;
        // bounded: a transport address is at most 512 bytes, plus one overflow byte.
        let mut bytes = Vec::with_capacity(513);
        (&mut file).take(513).read_to_end(&mut bytes).await?;
        let addr = CustomAddr::from_parts(IPC_TRANSPORT_ID, &bytes);
        if bytes.len() > 512 || !crate::backend::Backend::valid_addr(&addr) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid IPC rendezvous record",
            ));
        }
        Ok(Some(TransportAddr::Custom(addr)))
    }

    pub(crate) fn record_path(&self, id: EndpointId) -> PathBuf {
        self.root.join(format!("{id}.ipc"))
    }

    fn lease_path(&self, id: EndpointId) -> PathBuf {
        self.root.join(format!("{id}.lease"))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn socket_path(&self, id: EndpointId) -> PathBuf {
        self.root.join(format!("{id}.sock"))
    }

    pub(crate) async fn guard(&self) -> io::Result<Lock> {
        self.access.ensure(&self.root).await?;
        let path = self.root.join(".ipc.lock");
        // This single persistent lock serializes pathname creation/removal, so
        // nobody can acquire a lease on an unlinked inode. It is never removed.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match Lock::open(&path, true).await {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    result => return result,
                }
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC directory lock timeout"))?
    }
}

#[derive(Debug)]
pub(crate) struct Lease {
    discovery: LocalDiscovery,
    id: EndpointId,
    lock: Lock,
    #[cfg(target_os = "linux")]
    socket: Option<PathBuf>,
}

impl Lease {
    // Caller holds the directory guard until publication or failure cleanup.
    pub(crate) async fn acquire(discovery: LocalDiscovery, id: EndpointId) -> io::Result<Self> {
        let lock = Lock::open(&discovery.lease_path(id), true)
            .await
            .map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    io::Error::new(io::ErrorKind::AddrInUse, "IPC identity already bound")
                } else {
                    error
                }
            })?;
        #[cfg(target_os = "linux")]
        {
            // Only a valid record written by this transport grants ownership
            // of a stale socket pathname. Never delete an unrelated entry.
            match tokio::fs::File::open(discovery.record_path(id)).await {
                Ok(file) => {
                    // bounded: only a complete transport address can authorize cleanup.
                    let mut bytes = Vec::with_capacity(513);
                    file.take(513).read_to_end(&mut bytes).await?;
                    let old = CustomAddr::from_parts(IPC_TRANSPORT_ID, &bytes);
                    if crate::backend::Backend::path(&old).as_deref()
                        == Some(discovery.socket_path(id).as_path())
                    {
                        remove_if_present(&discovery.socket_path(id)).await?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            discovery,
            id,
            lock,
            #[cfg(target_os = "linux")]
            socket: None,
        })
    }

    pub(crate) async fn publish(&mut self, addr: &CustomAddr) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.socket = crate::backend::Backend::path(addr);
        }
        // Windows file locks exclude reads from other handles. Keep the locked
        // lease empty and store the readable address in a separate regular file.
        let path = self.discovery.record_path(self.id);
        tokio::fs::write(&path, addr.data()).await?;
        #[cfg(unix)]
        {
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).await?;
        }
        Ok(())
    }

    pub(crate) async fn release(self) -> io::Result<()> {
        let _guard = self.discovery.guard().await?;
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.socket {
            remove_if_present(path).await?;
        }
        // Close before unlink on Windows, where an open handle can forbid
        // deletion. The directory guard excludes all new binders/resolvers.
        let record = self.discovery.record_path(self.id);
        drop(self.lock);
        remove_if_present(&record).await?;
        remove_if_present(&self.discovery.lease_path(self.id)).await
    }
}

pub(crate) async fn remove_if_present(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
