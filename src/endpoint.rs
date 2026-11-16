//! Shared iroh interface; platform details stay inside the backend.

use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::task::{Context, Poll};

use iroh::endpoint::transports::{CustomEndpoint, CustomSender, RecvInfo, Transmit};
use iroh_base::CustomAddr;

use crate::backend::Backend;
use crate::transport::Inner;

#[derive(Debug)]
pub(crate) struct IpcEndpoint {
    inner: Arc<Inner>,
    watchable: n0_watcher::Watchable<Vec<CustomAddr>>,
}

impl IpcEndpoint {
    pub(crate) fn new(inner: Arc<Inner>) -> Self {
        // bounded: one backend address per endpoint.
        let watchable = n0_watcher::Watchable::new(vec![inner.addr.clone()]);
        Self { inner, watchable }
    }
}

impl CustomEndpoint for IpcEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.watchable.watch()
    }

    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(IpcSender(self.inner.clone()))
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        self.inner
            .backend
            .lock()
            .expect("backend lock poisoned")
            .as_ref()
            .map_or(NonZeroUsize::MIN, Backend::max_transmit_segments)
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let mut guard = self.inner.backend.lock().expect("backend lock poisoned");
        let Some(backend) = guard.as_mut() else {
            return Poll::Ready(Err(closed()));
        };
        backend.poll_recv(cx, bufs, metas, infos)
    }
}

#[derive(Debug)]
struct IpcSender(Arc<Inner>);

impl CustomSender for IpcSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        Backend::valid_addr(addr)
    }

    fn poll_send(
        &self,
        cx: &mut Context<'_>,
        dst: &CustomAddr,
        _src: Option<&CustomAddr>,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let mut guard = self.0.backend.lock().expect("backend lock poisoned");
        let Some(backend) = guard.as_mut() else {
            return Poll::Ready(Err(closed()));
        };
        backend.poll_send(cx, dst, transmit)
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "IPC transport closed")
}
