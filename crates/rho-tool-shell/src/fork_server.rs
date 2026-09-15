//! Native Bash launch protocol. A server has one immutable environment;
//! every command gets fresh Bash state and ordinary, independent pipe I/O.
use std::collections::HashMap;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::net::{RecvAncillaryBuffer, SendAncillaryBuffer, SendAncillaryMessage};
use tokio::io::unix::AsyncFd;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

type Pending = Arc<Mutex<HashMap<u64, Reply>>>;
struct Reply {
    started: Option<oneshot::Sender<io::Result<()>>>,
    completed: oneshot::Sender<ExitStatus>,
    _permit: OwnedSemaphorePermit,
}

pub(super) struct Server {
    socket: Arc<AsyncFd<OwnedFd>>,
    admission: tokio::sync::Mutex<()>,
    next: AtomicU64,
    permits: Arc<Semaphore>,
    pending: Pending,
    cancel: mpsc::UnboundedSender<u64>,
    stop: Option<oneshot::Sender<()>>,
    closed: Arc<AtomicBool>,
}
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashServer")
            .field("closed", &self.closed())
            .finish()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

pub(super) struct Job {
    server: Arc<Server>,
    id: u64,
    admitted: bool,
    cancel: bool,
    completed: oneshot::Receiver<ExitStatus>,
}
impl Drop for Job {
    fn drop(&mut self) {
        if !self.admitted {
            self.server.pending.lock().unwrap().remove(&self.id);
        } else {
            self.cancel();
        }
    }
}
impl Job {
    pub fn cancel(&mut self) {
        if std::mem::take(&mut self.cancel) {
            let _ = self.server.cancel.send(self.id);
        }
    }
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let result = (&mut self.completed)
            .await
            .map_err(|_| io::Error::other("Bash execution server disconnected"));
        self.cancel = false;
        result
    }
}

impl Server {
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub async fn start(mut command: tokio::process::Command) -> io::Result<Arc<Self>> {
        use rustix::net::{AddressFamily, SocketFlags, SocketType};
        let (parent, child_socket) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        let socket = Arc::new(AsyncFd::new(parent)?);
        // Preserve Bash's ordinary $0 and report the last failing pipeline
        // stage, rather than just the status of an output filter such as tail.
        command.as_std_mut().arg0("bash");
        command
            .args([
                "--noprofile",
                "--norc",
                "--rho-fork-server",
                "-o",
                "pipefail",
            ])
            .stdin(std::process::Stdio::from(child_socket))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .process_group(0);
        rho_fs_view::command_stdio_only(&mut command);
        let mut child = command.spawn()?;
        let ready = tokio::time::timeout(Duration::from_secs(30), receive(&socket))
            .await
            .map_err(io::Error::other)??;
        if ready != (0, 0, 1) {
            return Err(io::Error::other("incompatible Bash execution server"));
        }
        let pending: Pending = Arc::default();
        let (cancel, mut cancellations) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        let closed = Arc::new(AtomicBool::new(false));
        let mut reader: tokio::task::JoinHandle<io::Result<()>> = tokio::spawn({
            let socket = socket.clone();
            let pending = pending.clone();
            async move {
                loop {
                    let (kind, id, value) = receive(&socket).await?;
                    let mut pending = pending.lock().unwrap();
                    match kind {
                        1 => {
                            let reply = pending
                                .get_mut(&id)
                                .ok_or_else(|| io::Error::other("unknown Bash start reply"))?;
                            let started = reply
                                .started
                                .take()
                                .ok_or_else(|| io::Error::other("duplicate Bash start reply"))?;
                            let _ = started.send(Ok(()));
                        }
                        2 => {
                            let reply = pending
                                .remove(&id)
                                .ok_or_else(|| io::Error::other("unknown Bash completion"))?;
                            if reply.started.is_some() {
                                return Err(io::Error::other("Bash completed before startup"));
                            }
                            let _ = reply.completed.send(ExitStatus::from_raw(value as i32));
                        }
                        3 => {
                            let reply = pending
                                .remove(&id)
                                .ok_or_else(|| io::Error::other("unknown Bash failure"))?;
                            let started = reply.started.ok_or_else(|| {
                                io::Error::other("Bash startup failed after acceptance")
                            })?;
                            let _ = started.send(Err(io::Error::from_raw_os_error(value as i32)));
                        }
                        _ => return Err(io::Error::other("invalid Bash response")),
                    }
                }
            }
        });
        let mut canceller = tokio::spawn({
            let socket = socket.clone();
            async move {
                while let Some(id) = cancellations.recv().await {
                    send(&socket, 2, id, &[], &[]).await?;
                }
                Ok::<(), io::Error>(())
            }
        });
        tokio::spawn({
            let socket = socket.clone();
            let pending = pending.clone();
            let closed = closed.clone();
            async move {
                tokio::select! { _=stopped=>{}, _=&mut reader=>{}, _=&mut canceller=>{} }
                closed.store(true, Ordering::Release);
                // EOF is the native supervisor's graceful cancel-and-reap path.
                let _ = rustix::net::shutdown(socket.get_ref(), rustix::net::Shutdown::Both);
                reader.abort();
                canceller.abort();
                if tokio::time::timeout(Duration::from_secs(5), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
                pending.lock().unwrap().clear();
            }
        });
        Ok(Arc::new(Self {
            socket,
            admission: tokio::sync::Mutex::new(()),
            next: AtomicU64::new(1),
            permits: Arc::new(Semaphore::new(64)),
            pending,
            cancel,
            stop: Some(stop),
            closed,
        }))
    }

    pub async fn spawn(
        self: &Arc<Self>,
        source: &str,
        cwd: &Path,
        fds: &[OwnedFd; 3],
    ) -> io::Result<Job> {
        let cwd = cwd.as_os_str().as_bytes();
        if cwd.is_empty()
            || cwd.len() > 4096
            || source.len() > 131071
            || cwd.contains(&0)
            || source.as_bytes().contains(&0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Bash command or working directory",
            ));
        }
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(io::Error::other)?;
        // Keep IDs and packet admission ordered. Cancellation cannot leave an
        // unsent request owning capacity, or an admitted job without cleanup.
        let _admission = self.admission.lock().await;
        if self.closed() {
            return Err(io::Error::other("Bash execution server closed"));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (started, ready) = oneshot::channel();
        let (completed, done) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id,
            Reply {
                started: Some(started),
                completed,
                _permit: permit,
            },
        );
        let mut job = Job {
            server: self.clone(),
            id,
            admitted: false,
            cancel: true,
            completed: done,
        };
        let mut payload = Vec::with_capacity(4 + cwd.len() + source.len());
        payload.extend_from_slice(&(cwd.len() as u32).to_be_bytes());
        payload.extend_from_slice(cwd);
        payload.extend_from_slice(source.as_bytes());
        send(&self.socket, 1, id, &payload, fds).await?;
        job.admitted = true;
        drop(_admission);
        ready
            .await
            .map_err(|_| io::Error::other("Bash server closed during command startup"))??;
        Ok(job)
    }
}

async fn send(
    socket: &AsyncFd<OwnedFd>,
    kind: u32,
    id: u64,
    payload: &[u8],
    fds: &[OwnedFd],
) -> io::Result<()> {
    let mut header = [0u8; 16];
    header[..4].copy_from_slice(&kind.to_be_bytes());
    header[4..12].copy_from_slice(&id.to_be_bytes());
    header[12..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    let descriptors = fds.iter().map(AsFd::as_fd).collect::<Vec<_>>();
    loop {
        let mut ready = socket.writable().await?;
        let result = ready.try_io(|socket| {
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            if !descriptors.is_empty() {
                control.push(SendAncillaryMessage::ScmRights(&descriptors));
            }
            let n = rustix::net::sendmsg(
                socket.get_ref(),
                &[IoSlice::new(&header), IoSlice::new(payload)],
                &mut control,
                rustix::net::SendFlags::NOSIGNAL,
            )?;
            if n != 16 + payload.len() {
                return Err(io::Error::other("short Bash packet"));
            }
            Ok(())
        });
        if let Ok(result) = result {
            return result;
        }
    }
}

async fn receive(socket: &AsyncFd<OwnedFd>) -> io::Result<(u32, u64, u32)> {
    loop {
        let mut ready = socket.readable().await?;
        let result = ready.try_io(|socket| {
            let mut bytes = [0u8; 16];
            let mut control = RecvAncillaryBuffer::new(&mut []);
            let received = rustix::net::recvmsg(
                socket.get_ref(),
                &mut [IoSliceMut::new(&mut bytes)],
                &mut control,
                rustix::net::RecvFlags::CMSG_CLOEXEC,
            )?;
            if received.bytes != 16
                || received
                    .flags
                    .intersects(rustix::net::ReturnFlags::TRUNC | rustix::net::ReturnFlags::CTRUNC)
            {
                return Err(io::Error::other(format!(
                    "invalid Bash response: {} bytes, {:?}",
                    received.bytes, received.flags
                )));
            }
            Ok((
                u32::from_be_bytes(bytes[..4].try_into().unwrap()),
                u64::from_be_bytes(bytes[4..12].try_into().unwrap()),
                u32::from_be_bytes(bytes[12..].try_into().unwrap()),
            ))
        });
        if let Ok(result) = result {
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    fn command() -> tokio::process::Command {
        tokio::process::Command::new(format!("{}/bin/rho-bash", rho_fs_view::AGENT_BASE))
    }

    async fn run(server: &Arc<Server>, cwd: &Path, source: &str) -> (ExitStatus, Vec<u8>, Vec<u8>) {
        let (out, mut stdout) = tokio::net::unix::pipe::pipe().unwrap();
        let (err, mut stderr) = tokio::net::unix::pipe::pipe().unwrap();
        let fds = [
            std::fs::File::open("/dev/null").unwrap().into(),
            out.into_blocking_fd().unwrap(),
            err.into_blocking_fd().unwrap(),
        ];
        let mut job = server.spawn(source, cwd, &fds).await.unwrap();
        drop(fds);
        let mut output = Vec::new();
        let mut errors = Vec::new();
        let (status, _, _) = tokio::try_join!(
            job.wait(),
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut errors)
        )
        .unwrap();
        (status, output, errors)
    }

    #[tokio::test]
    async fn independent_bash_startup_and_large_binary_output() {
        let dir = tempfile::tempdir().unwrap();
        let startup = dir.path().join("startup");
        let log = dir.path().join("startup-log");
        std::fs::write(
            &startup,
            format!("echo start >> '{}'; export STARTED=yes\n", log.display()),
        )
        .unwrap();
        let mut command = command();
        command.env("BASH_ENV", &startup).current_dir(dir.path());
        let server = Server::start(command).await.unwrap();
        assert!(
            !log.exists(),
            "the supervisor must never evaluate startup code"
        );
        let (_,out,_)=run(&server,dir.path(),"printf '%s:%s:%s' \"$0\" \"$STARTED\" \"$(( $$ == BASHPID ))\"; export MUTATED=yes; cd /").await;
        assert_eq!(out, b"bash:yes:1");
        let (_, out, _) = run(
            &server,
            dir.path(),
            "printf '%s:%s' \"${MUTATED-unset}\" \"$PWD\"",
        )
        .await;
        assert_eq!(out, format!("unset:{}", dir.path().display()).as_bytes());
        let (status, out, err) = run(
            &server,
            dir.path(),
            "head -c 262144 /dev/zero; printf error >&2; exit 7",
        )
        .await;
        assert_eq!(status.code(), Some(7));
        assert_eq!(out, vec![0; 262144]);
        assert_eq!(err, b"error");
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "start\nstart\nstart\n"
        );
    }

    #[tokio::test]
    async fn warmed_children_refresh_identity_cwd_and_execution_string() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let mut command = command();
        command.current_dir(dir.path());
        let server = Server::start(command).await.unwrap();
        let source = r#"printf '%s\n' "$$" "$BASHPID" "$PPID" "$PWD" "$BASH_EXECUTION_STRING" "$RANDOM:$RANDOM:$RANDOM""#;
        let mut children = Vec::new();
        for cwd in [dir.path(), other.as_path()] {
            let (status, out, err) = run(&server, cwd, source).await;
            assert!(status.success(), "{err:?}");
            let fields = String::from_utf8(out).unwrap();
            let fields: Vec<_> = fields.lines().map(str::to_owned).collect();
            assert_eq!(fields[0], fields[1]);
            assert_ne!(fields[2].parse::<u32>().unwrap(), std::process::id());
            assert_eq!(fields[3], cwd.to_str().unwrap());
            assert_eq!(fields[4], source);
            children.push(fields);
        }
        assert_ne!(children[0][0], children[1][0]);
        assert_eq!(children[0][2], children[1][2]);
        assert_ne!(children[0][5], children[1][5]);
    }

    #[tokio::test]
    async fn descriptor_passed_pty_remains_a_noninteractive_command() {
        use std::io::Read;

        use rustix::pty::{OpenptFlags, ioctl_tiocgptpeer, openpt, unlockpt};

        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
        let master = openpt(flags).unwrap();
        unlockpt(&master).unwrap();
        let slave = ioctl_tiocgptpeer(&master, flags).unwrap();
        let mut master = std::fs::File::from(master);
        let fds = [
            rustix::io::dup(&slave).unwrap(),
            rustix::io::dup(&slave).unwrap(),
            slave,
        ];
        let server = Server::start(command()).await.unwrap();
        let mut job = server
            .spawn(
                "[[ -t 0 && -t 1 && -t 2 && $- != *i* ]] && printf pty-ok",
                Path::new("/"),
                &fds,
            )
            .await
            .unwrap();
        drop(fds);
        assert!(job.wait().await.unwrap().success());
        let mut output = [0; 6];
        master.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"pty-ok");
    }

    #[tokio::test]
    async fn shell_time_starts_at_command_admission() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = command();
        command.env_remove("SECONDS");
        let server = Server::start(command).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2100)).await;
        let (_, output, _) = run(&server, dir.path(), "printf %s \"$SECONDS\"").await;
        assert!(
            std::str::from_utf8(&output)
                .unwrap()
                .parse::<u64>()
                .unwrap()
                < 2
        );
    }

    #[tokio::test]
    async fn commands_run_concurrently_and_cancellation_reaps_the_leader() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(command()).await.unwrap();
        let marker = dir.path().join("leader");
        let fds = std::array::from_fn(|_| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .unwrap()
                .into()
        });
        let mut slow = server
            .spawn(
                &format!("echo $$ > '{}'; sleep 30 & wait", marker.display()),
                dir.path(),
                &fds,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let pid = std::fs::read_to_string(marker).unwrap();
        let quick = tokio::time::timeout(
            Duration::from_secs(2),
            run(&server, dir.path(), "echo independent"),
        )
        .await
        .unwrap();
        assert_eq!(quick.1, b"independent\n");
        slow.cancel();
        let status = tokio::time::timeout(Duration::from_secs(2), slow.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.signal(), Some(9));
        assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists());
        assert_eq!(server.permits.available_permits(), 64);
    }

    #[tokio::test]
    async fn aborted_admission_and_dropped_jobs_release_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(command()).await.unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..80 {
            let server = server.clone();
            let cwd = dir.path().to_owned();
            tasks.spawn(async move {
                let fds = std::array::from_fn(|_| {
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/dev/null")
                        .unwrap()
                        .into()
                });
                let _job = server.spawn("sleep 30", &cwd, &fds).await;
            });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        tokio::time::timeout(Duration::from_secs(3), async {
            while server.permits.available_permits() != 64 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert!(!server.closed());
        assert!(run(&server, dir.path(), "true").await.0.success());
    }

    #[tokio::test]
    async fn descriptors_and_invalid_requests_do_not_escape_their_scope() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("private-descriptor");
        let private = std::fs::File::create(&marker).unwrap();
        rustix::io::fcntl_setfd(&private, rustix::io::FdFlags::empty()).unwrap();
        let server = Server::start(command()).await.unwrap();
        let (status, output, _) = run(
            &server,
            dir.path(),
            "for fd in /proc/$$/fd/*; do readlink \"$fd\" || :; done",
        )
        .await;
        assert!(status.success());
        assert!(!String::from_utf8_lossy(&output).contains("private-descriptor"));
        let fds = std::array::from_fn(|_| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .unwrap()
                .into()
        });
        assert!(
            server
                .spawn("true", &dir.path().join("missing"), &fds)
                .await
                .is_err()
        );
        assert!(server.spawn("bad\0source", dir.path(), &fds).await.is_err());
        assert!(
            server
                .spawn(&"x".repeat(131072), dir.path(), &fds)
                .await
                .is_err()
        );
        assert!(run(&server, dir.path(), "true").await.0.success());
        // The native endpoint rejects malformed packets and retires rather
        // than interpreting incomplete input or attempting a shell fallback.
        send(&server.socket, 99, 0, &[], &[]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !server.closed() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn server_death_fails_without_replaying_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(command()).await.unwrap();
        let marker = dir.path().join("effects");
        let fds = std::array::from_fn(|_| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .unwrap()
                .into()
        });
        let mut job = server
            .spawn(
                &format!(
                    "echo once >> '{}'; kill -KILL $PPID; sleep 30",
                    marker.display()
                ),
                dir.path(),
                &fds,
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), job.wait())
                .await
                .unwrap()
                .is_err()
        );
        assert!(server.closed());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "once\n");
        assert!(server.spawn("true", dir.path(), &fds).await.is_err());
    }
}
