//! Filesystem rendezvous, with leases instead of socket liveness probes.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iroh::{EndpointAddr, EndpointId, TransportAddr};
use iroh_base::CustomAddr;
use tokio::io::AsyncReadExt;

use crate::IPC_TRANSPORT_ID;
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
}

impl LocalDiscovery {
    /// Choose a rendezvous directory private to the current user. If missing,
    /// it is created on first use with mode `0700`, independent of the umask.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The rendezvous directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Remove the records of owners that are gone: a lease nobody holds,
    /// and the record and socket beside it. Returns how many were removed.
    /// A crash leaves such records; the next owner of the same identity
    /// reclaims its own, and this reclaims everyone's.
    pub async fn sweep(&self) -> io::Result<usize> {
        if !tokio::fs::try_exists(&self.root).await? {
            return Ok(0);
        }
        let _guard = self.guard().await?;
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut removed = 0;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("lease") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<EndpointId>().ok())
            else {
                continue;
            };
            match Lock::open(&path, false).await {
                // Held: its owner lives.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
                Ok(lock) => {
                    remove_if_present(&self.socket_path(id)).await?;
                    remove_if_present(&self.record_path(id)).await?;
                    drop(lock);
                    remove_if_present(&path).await?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
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
        // bounded: a transport address is at most 512 bytes, plus one overflow
        // byte.
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

    /// Watch the process that owns `id` now, to learn when it has gone.
    /// `None` when no live process owns it.
    ///
    /// A crashed owner sends nothing to say so, and a successor bound to the
    /// same identity takes over its socket: what its peers sent to the old
    /// process now reaches one that does not know them. The watch is of the
    /// owner, not of the identity, so it tells the two apart.
    pub async fn watch(&self, id: EndpointId) -> io::Result<Option<OwnerWatch>> {
        if !tokio::fs::try_exists(&self.root).await? {
            return Ok(None);
        }
        let _guard = self.guard().await?;
        let lease = match tokio::fs::File::open(self.lease_path(id)).await {
            Ok(file) => std::sync::Arc::new(file.into_std().await),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !crate::locking::held_elsewhere(lease.clone()).await? {
            return Ok(None);
        }
        Ok(Some(OwnerWatch {
            discovery: self.clone(),
            lease,
        }))
    }

    pub(crate) fn record_path(&self, id: EndpointId) -> PathBuf {
        self.root.join(format!("{id}.ipc"))
    }

    fn lease_path(&self, id: EndpointId) -> PathBuf {
        self.root.join(format!("{id}.lease"))
    }

    /// The socket carries only a prefix of the id: an AF_UNIX pathname is
    /// at most 107 bytes, and the full 64-character id leaves too few for
    /// the directory. The record and lease, which are looked up by id, keep
    /// the whole of it.
    pub(crate) fn socket_path(&self, id: EndpointId) -> PathBuf {
        self.root.join(format!("{:.16}.sock", id.to_string()))
    }

    pub(crate) async fn guard(&self) -> io::Result<Lock> {
        crate::directory_access::ensure_private(&self.root).await?;
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

/// The process that owned an identity when [`LocalDiscovery::watch`] looked.
///
/// It keeps that owner's lease file open. Every owner locks a lease file of
/// its own, so a successor never holds this one.
#[derive(Debug)]
pub struct OwnerWatch {
    discovery: LocalDiscovery,
    lease: std::sync::Arc<std::fs::File>,
}

impl OwnerWatch {
    /// Whether the owner watched still runs. Once it has gone this stays
    /// false, also after another process binds the same identity.
    pub async fn lives(&self) -> io::Result<bool> {
        let _guard = self.discovery.guard().await?;
        crate::locking::held_elsewhere(self.lease.clone()).await
    }
}

#[derive(Debug)]
pub(crate) struct Lease {
    discovery: LocalDiscovery,
    id: EndpointId,
    lock: Lock,
    socket: Option<PathBuf>,
}

impl Lease {
    // Caller holds the directory guard until publication or failure cleanup.
    pub(crate) async fn acquire(discovery: LocalDiscovery, id: EndpointId) -> io::Result<Self> {
        // A lease file left by an owner that is gone goes first: each owner
        // locks a file of its own, so a watch of the old owner
        // ([`OwnerWatch`]) never sees the new one in it.
        match Lock::open(&discovery.lease_path(id), false).await {
            Ok(stale) => {
                drop(stale);
                remove_if_present(&discovery.lease_path(id)).await?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
        let lock = Lock::open(&discovery.lease_path(id), true)
            .await
            .map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    io::Error::new(io::ErrorKind::AddrInUse, "IPC identity already bound")
                } else {
                    error
                }
            })?;
        {
            // Only a valid record written by this transport grants ownership
            // of a stale socket pathname. Never delete an unrelated entry.
            match tokio::fs::File::open(discovery.record_path(id)).await {
                Ok(file) => {
                    // bounded: only a complete transport address can authorize
                    // cleanup.
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
            socket: None,
        })
    }

    pub(crate) async fn publish(&mut self, addr: &CustomAddr) -> io::Result<()> {
        {
            self.socket = crate::backend::Backend::path(addr);
        }
        // The lease stays empty and locked; the address is a regular file
        // beside it that anyone resolving reads.
        let path = self.discovery.record_path(self.id);
        tokio::fs::write(&path, addr.data()).await?;
        crate::directory_access::make_private(&path).await?;
        Ok(())
    }

    pub(crate) async fn release(self) -> io::Result<()> {
        let _guard = self.discovery.guard().await?;
        if let Some(path) = &self.socket {
            remove_if_present(path).await?;
        }
        // Close before unlink, as the upstream did for Windows, where an open
        // handle can forbid deletion. The directory guard excludes all new
        // binders/resolvers.
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
