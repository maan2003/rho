//! RAM-only secret storage for platform tokens.
//!
//! Secrets live in a sealed memfd: never on disk, no filesystem name, and
//! `CLOEXEC` so spawned children (including model-authored code) cannot
//! inherit it. Restart survival uses the systemd fd store: the memfd is
//! stashed with `FDSTORE=1` and comes back via `$LISTEN_FDS` on the next
//! start. The store is per-boot — after a reboot the user re-initializes.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::FileExt as _;

use anyhow::{Context as _, bail};

/// First fd passed by systemd via `$LISTEN_FDS` (SD_LISTEN_FDS_START).
const LISTEN_FDS_START: i32 = 3;

pub struct SecretStore {
    memfd: File,
}

impl SecretStore {
    /// Write `secrets` into a fresh sealed memfd.
    pub fn create(secrets: &BTreeMap<String, String>) -> anyhow::Result<Self> {
        let payload = serde_json::to_vec(secrets).context("encoding secrets")?;
        let fd = unsafe {
            libc::memfd_create(
                c"rho-platform-secrets".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd == -1 {
            return Err(std::io::Error::last_os_error()).context("memfd_create");
        }
        // SAFETY: fd is a fresh, owned memfd.
        let mut memfd = unsafe { File::from_raw_fd(fd) };
        memfd.write_all(&payload).context("writing secrets")?;
        let seals =
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
        if unsafe { libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, seals) } == -1 {
            return Err(std::io::Error::last_os_error()).context("sealing secrets memfd");
        }
        Ok(Self { memfd })
    }

    /// Adopt an fd that is already a sealed secrets memfd (e.g. from the fd
    /// store). Contents are validated on `read`, not here.
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self {
            memfd: File::from(fd),
        }
    }

    /// Decode the stored secrets. The token values enter this process's heap
    /// here; keep the returned map short-lived and never log it.
    pub fn read(&self) -> anyhow::Result<BTreeMap<String, String>> {
        let len = self.memfd.metadata().context("memfd metadata")?.len();
        let mut buf = vec![0u8; usize::try_from(len).context("memfd size")?];
        self.memfd
            .read_exact_at(&mut buf, 0)
            .context("reading secrets memfd")?;
        serde_json::from_slice(&buf).context("decoding secrets memfd")
    }

    /// Reclaim a stashed store from systemd's fd store by `FDNAME`.
    ///
    /// Returns `Ok(None)` when not running under systemd, when `$LISTEN_PID`
    /// is another process, or when no stored fd carries `name`. Env vars are
    /// left in place; callers taking multiple fds parse once.
    pub fn take_from_listen_fds(name: &str) -> anyhow::Result<Option<Self>> {
        let Ok(listen_pid) = std::env::var("LISTEN_PID") else {
            return Ok(None);
        };
        if listen_pid.trim() != std::process::id().to_string() {
            return Ok(None);
        }
        let count: i32 = std::env::var("LISTEN_FDS")
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        let names = std::env::var("LISTEN_FDNAMES").unwrap_or_default();
        for (index, fd_name) in names.split(':').take(count as usize).enumerate() {
            if fd_name != name {
                continue;
            }
            let raw = LISTEN_FDS_START + index as i32;
            // systemd passes stored fds without CLOEXEC; restore it so the
            // memfd never leaks into children.
            let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
            if flags == -1
                || unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
            {
                return Err(std::io::Error::last_os_error()).context("claiming stored fd");
            }
            // SAFETY: systemd handed this fd to us and nothing else in the
            // process claims LISTEN_FDS entries by this name.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            return Ok(Some(Self::from_fd(fd)));
        }
        Ok(None)
    }

    /// Stash the memfd in the systemd fd store under `name`.
    ///
    /// Returns false when `$NOTIFY_SOCKET` is unset (not under systemd, or
    /// `NotifyAccess` not granted); the caller decides whether that is fatal.
    /// The unit needs `FileDescriptorStoreMax=` for systemd to accept it.
    pub fn stash_in_fd_store(&self, name: &str) -> anyhow::Result<bool> {
        if name.contains(|c: char| c == ':' || c.is_whitespace()) {
            bail!("fd store name must not contain ':' or whitespace: {name:?}");
        }
        let Ok(socket_path) = std::env::var("NOTIFY_SOCKET") else {
            return Ok(false);
        };
        let message = format!("FDSTORE=1\nFDNAME={name}");
        send_with_fd(&socket_path, message.as_bytes(), self.memfd.as_raw_fd())
            .context("sending FDSTORE notification")?;
        Ok(true)
    }
}

/// Send a datagram with one SCM_RIGHTS fd to a notify socket. Supports
/// filesystem paths and abstract addresses (leading '@', systemd convention).
fn send_with_fd(socket_path: &str, payload: &[u8], fd: i32) -> anyhow::Result<()> {
    let sock = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if sock == -1 {
        return Err(std::io::Error::last_os_error()).context("notify socket");
    }
    // SAFETY: sock is a fresh, owned socket fd.
    let sock = unsafe { OwnedFd::from_raw_fd(sock) };

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = socket_path.as_bytes();
    if bytes.is_empty() || bytes.len() >= addr.sun_path.len() {
        bail!("bad NOTIFY_SOCKET path: {socket_path:?}");
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    if bytes[0] == b'@' {
        addr.sun_path[0] = 0;
    }
    let addr_len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len()) as libc::socklen_t;

    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };
    const CMSG_SPACE: usize = 24; // CMSG_SPACE(sizeof(int)) on 64-bit linux
    let mut cmsg_buf = [0u8; CMSG_SPACE];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = &mut addr as *mut _ as *mut libc::c_void;
    msg.msg_namelen = addr_len;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as _;

    // SAFETY: cmsg buffer is sized and aligned per CMSG_* macros.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            &fd as *const i32 as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<i32>(),
        );
        if libc::sendmsg(sock.as_raw_fd(), &msg, 0) == -1 {
            return Err(std::io::Error::last_os_error()).context("sendmsg");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("SLACK_BOT_TOKEN".to_string(), "xoxb-test".to_string()),
            ("SLACK_APP_TOKEN".to_string(), "xapp-test".to_string()),
        ])
    }

    #[test]
    fn round_trip_and_sealed() {
        let store = SecretStore::create(&sample()).unwrap();
        assert_eq!(store.read().unwrap(), sample());
        // The seals must reject any further writes.
        let err = (&store.memfd).write_all(b"x").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn stash_sends_fd_and_fdstore_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let receiver = std::os::unix::net::UnixDatagram::bind(&path).unwrap();

        let store = SecretStore::create(&sample()).unwrap();
        // Not under systemd -> no-op.
        // SAFETY: test process is single-threaded at this point.
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        assert!(!store.stash_in_fd_store("slack").unwrap());

        unsafe { std::env::set_var("NOTIFY_SOCKET", &path) };
        assert!(store.stash_in_fd_store("slack").unwrap());
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };

        let (payload, fd) = recv_with_fd(&receiver);
        assert_eq!(payload, "FDSTORE=1\nFDNAME=slack");
        let restored = SecretStore::from_fd(fd);
        assert_eq!(restored.read().unwrap(), sample());
    }

    /// Test-side recvmsg with SCM_RIGHTS (std exposes no ancillary API).
    fn recv_with_fd(sock: &std::os::unix::net::UnixDatagram) -> (String, OwnedFd) {
        let mut buf = [0u8; 256];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut cmsg_buf = [0u8; 64];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_buf.len();
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
        assert!(
            n >= 0,
            "recvmsg failed: {}",
            std::io::Error::last_os_error()
        );
        let payload = String::from_utf8(buf[..n as usize].to_vec()).unwrap();
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        assert!(!cmsg.is_null(), "no control message");
        let mut fd: i32 = -1;
        unsafe {
            assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
            std::ptr::copy_nonoverlapping(
                libc::CMSG_DATA(cmsg),
                &mut fd as *mut i32 as *mut u8,
                std::mem::size_of::<i32>(),
            );
        }
        assert!(fd >= 0);
        (payload, unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
