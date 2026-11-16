//! Prepared transport factory and owned shutdown boundary.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use iroh::EndpointId;
use iroh::endpoint::transports::{CustomEndpoint, CustomTransport};
use iroh_base::CustomAddr;

use crate::LocalDiscovery;
use crate::backend::Backend;
use crate::discovery::Lease;
use crate::endpoint::IpcEndpoint;

/// A prepared, single-bind local transport and its ownership lease.
///
/// Preparation is async because iroh's synchronous `CustomTransport::bind`
/// cannot perform filesystem rendezvous safely. Clones share the same resource;
/// binding a clone a second time fails instead of creating competing receivers.
#[derive(Debug, Clone)]
pub struct IpcTransport {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) backend: Mutex<Option<Backend>>,
    pub(crate) addr: CustomAddr,
    lease: Mutex<Option<Lease>>,
    close: tokio::sync::Mutex<()>,
    bound: AtomicBool,
}

impl IpcTransport {
    /// Bind this platform's local datagram backend and publish an exclusive lease.
    /// A concurrently live owner of `id` causes `AddrInUse`.
    pub async fn prepare(discovery: LocalDiscovery, id: EndpointId) -> io::Result<Self> {
        let guard = discovery.guard().await?;
        let mut lease = Lease::acquire(discovery.clone(), id).await?;
        let backend = match Backend::bind(&discovery, id).await {
            Ok(backend) => backend,
            Err(error) => {
                drop(guard);
                lease.release().await?;
                return Err(error);
            }
        };
        let addr = backend.local_addr();
        if let Err(error) = lease.publish(&addr).await {
            backend.close().await;
            drop(guard);
            lease.release().await?;
            return Err(error);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                backend: Mutex::new(Some(backend)),
                addr,
                lease: Mutex::new(Some(lease)),
                close: tokio::sync::Mutex::new(()),
                bound: AtomicBool::new(false),
            }),
        })
    }

    /// Opaque native local address, suitable for `TransportAddr::Custom`.
    pub fn local_addr(&self) -> CustomAddr {
        self.inner.addr.clone()
    }

    /// Close the OS transport, then remove its discovery record and release ownership.
    /// Call after iroh's endpoint has finished closing; concurrent calls are serialized.
    pub async fn close(&self) -> io::Result<()> {
        let _close = self.inner.close.lock().await;
        let backend = self
            .inner
            .backend
            .lock()
            .expect("backend lock poisoned")
            .take();
        if let Some(backend) = backend {
            backend.close().await;
        }
        let lease = self.inner.lease.lock().expect("lease lock poisoned").take();
        if let Some(lease) = lease {
            lease.release().await?;
        }
        Ok(())
    }
}

impl CustomTransport for IpcTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        if self.inner.bound.swap(true, Ordering::AcqRel) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "IPC transport already bound",
            ));
        }
        if self
            .inner
            .backend
            .lock()
            .expect("backend lock poisoned")
            .is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "IPC transport closed",
            ));
        }
        Ok(Box::new(IpcEndpoint::new(self.inner.clone())))
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let backend = self
            .backend
            .get_mut()
            .expect("backend lock poisoned")
            .take();
        let lease = self.lease.get_mut().expect("lease lock poisoned").take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(backend) = backend {
                    backend.close().await;
                }
                if let Some(lease) = lease
                    && let Err(error) = lease.release().await
                {
                    tracing::error!(%error, "IPC lease cleanup failed");
                }
            });
        }
        // Without a runtime, dropping OS handles releases the lease. The stale
        // record is ignored by lookup and reclaimed by the next prepare call.
    }
}
