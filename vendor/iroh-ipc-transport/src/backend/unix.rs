//! Linux Unix datagrams. This module is never compiled on macOS.

use std::ffi::OsStr;
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::task::{Context, Poll};

use iroh::EndpointId;
use iroh::endpoint::transports::{RecvInfo, Transmit};
use iroh_base::CustomAddr;
use tokio::io::ReadBuf;
use tokio::net::UnixDatagram;

use crate::{IPC_TRANSPORT_ID, LocalDiscovery};

#[derive(Debug)]
pub(crate) struct Backend {
    socket: UnixDatagram,
    addr: CustomAddr,
}

impl Backend {
    pub(crate) async fn bind(discovery: &LocalDiscovery, id: EndpointId) -> io::Result<Self> {
        let path = discovery.socket_path(id);
        let bind_path = path.clone();
        // Audited async boundary: bind(2) creates a filesystem socket inode;
        // Tokio's UnixDatagram::bind is synchronous. Keep that operation on
        // the blocking pool until Tokio provides async pathname binding.
        let socket = tokio::task::spawn_blocking(move || {
            let socket = std::os::unix::net::UnixDatagram::bind(bind_path)?;
            socket.set_nonblocking(true)?;
            Ok::<_, io::Error>(socket)
        })
        .await
        .map_err(io::Error::other)??;
        let socket = UnixDatagram::from_std(socket)?;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).await?;
        let sock = socket2::SockRef::from(&socket);
        // OS limits may clamp these hints; correctness relies on readiness,
        // bounded batch draining and QUIC congestion control, not buffer size.
        if let Err(error) = sock.set_recv_buffer_size(8 * 1024 * 1024) {
            tracing::debug!(%error, "kernel retained default IPC receive buffer");
        }
        if let Err(error) = sock.set_send_buffer_size(8 * 1024 * 1024) {
            tracing::debug!(%error, "kernel retained default IPC send buffer");
        }
        Ok(Self {
            socket,
            addr: address(&path),
        })
    }

    pub(crate) fn local_addr(&self) -> CustomAddr {
        self.addr.clone()
    }
    pub(crate) fn valid_addr(addr: &CustomAddr) -> bool {
        Self::path(addr).is_some()
    }
    pub(crate) fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
    pub(crate) async fn close(self) {
        drop(self);
    }

    pub(crate) fn path(addr: &CustomAddr) -> Option<PathBuf> {
        if addr.id() != IPC_TRANSPORT_ID {
            return None;
        }
        let (&2, bytes) = addr.data().split_first()? else {
            return None;
        };
        if bytes.is_empty() || bytes.len() >= 108 || bytes.contains(&0) {
            return None;
        }
        let path = PathBuf::from(OsStr::from_bytes(bytes));
        path.is_absolute().then_some(path)
    }

    pub(crate) fn poll_send(
        &mut self,
        cx: &mut Context<'_>,
        dst: &CustomAddr,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(path) = Self::path(dst) else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Unix datagram address",
            )));
        };
        self.socket
            .poll_send_to(cx, transmit.contents, path)
            .map(|result| {
                result.and_then(|count| {
                    if count == transmit.contents.len() {
                        Ok(())
                    } else {
                        Err(io::Error::new(io::ErrorKind::WriteZero, "partial datagram"))
                    }
                })
            })
    }

    pub(crate) fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let cap = bufs.len().min(metas.len()).min(infos.len());
        let mut count = 0;
        // Bound the work even when an unnamed sender floods invalid packets.
        for _ in 0..cap {
            let mut read = ReadBuf::new(&mut bufs[count]);
            match self.socket.poll_recv_from(cx, &mut read) {
                Poll::Pending if count == 0 => return Poll::Pending,
                Poll::Pending => break,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(source)) => {
                    let Some(path) = source.as_pathname() else {
                        continue;
                    };
                    metas[count] = noq_udp::RecvMeta::default();
                    metas[count].len = read.filled().len();
                    metas[count].stride = read.filled().len();
                    infos[count] = RecvInfo::new(address(path), Some(self.addr.clone()));
                    count += 1;
                }
            }
        }
        if count == 0 && cap != 0 {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(Ok(count))
    }
}

fn address(path: &Path) -> CustomAddr {
    // bounded: AF_UNIX pathname limit is 108 bytes.
    let mut bytes = Vec::with_capacity(109);
    bytes.push(2);
    bytes.extend_from_slice(path.as_os_str().as_bytes());
    CustomAddr::from_parts(IPC_TRANSPORT_ID, &bytes)
}
