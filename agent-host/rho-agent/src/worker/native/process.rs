//! Planned notebook snapshots. The Python interpreter, shell tools, threads,
//! and open files remain in the same child; only typed observations cross the
//! socket to its owner. The `current` marker is published after a full dump.
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use rho_agent_types::{AgentId, AgentRole};
use rho_notebook::process::{self, Client, Event, ServiceClient, ServiceKind, ServiceRequest};
use rho_notebook::{Export, Notebook};
use rho_tool_shell::ShellTools;
use rho_web_search::{Credentials, CredentialsProvider};
use senax_encoder::{Decode, Encode};
use tokio::sync::{Notify, mpsc};

use crate::ipc::protocol::SharedCall;
use crate::worker::shared::mailroom::{Mailroom, Outbound};
use crate::worker::shared::tools::{AgentHost, host_tools_with_services};

#[derive(Clone, Encode, Decode)]
struct Startup {
    cwd: camino::Utf8PathBuf,
    role: AgentRole,
    id: AgentId,
    has_team: bool,
}

/// Only the credential fields required by `web.run`, not a persisted token.
#[derive(Encode, Decode)]
pub(crate) struct CredentialsWire {
    pub bearer_token: String,
    pub account_id: Option<String>,
}

pub(crate) struct Process {
    pub(crate) client: Arc<Client>,
    guardian: Mutex<UnixStream>,
    dump_attempted: std::sync::atomic::AtomicBool,
}

impl Process {
    /// Return whether the child came from a snapshot. The caller resumes a
    /// restored child after its log and outbound event handling are ready.
    pub(crate) fn start(
        dir: &Path,
        cwd: &camino::Utf8Path,
        role: AgentRole,
        id: AgentId,
        has_team: bool,
        wake: Arc<Notify>,
        events_tx: mpsc::UnboundedSender<(u64, Event)>,
        services_tx: mpsc::UnboundedSender<ServiceRequest>,
    ) -> Result<(Self, bool)> {
        let startup = Startup {
            cwd: cwd.to_owned(),
            role,
            id,
            has_team,
        };
        let (socket, guardian, restored) = start_guardian(dir, &startup)?;
        let client = Arc::new(
            Client::connect_with_services(socket, wake, Some(events_tx), Some(services_tx))
                .map_err(anyhow::Error::msg)?,
        );
        Ok((
            Self {
                client,
                guardian: Mutex::new(guardian),
                dump_attempted: std::sync::atomic::AtomicBool::new(false),
            },
            restored,
        ))
    }

    pub(crate) fn checkpoint(&self) -> Result<()> {
        // A failed dump may stop the child. Do not ask it for a graceful
        // shutdown after this point, even if the guardian reports an error.
        self.dump_attempted
            .store(true, std::sync::atomic::Ordering::Release);
        control(&mut self.guardian.lock().unwrap(), b'C').map(|_| ())
    }

    pub(crate) fn shutdown(&self) -> Result<()> {
        if self
            .dump_attempted
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return control(&mut self.guardian.lock().unwrap(), b'S').map(|_| ());
        }
        let answer = self.client.shutdown().map_err(anyhow::Error::msg);
        // The guardian reaps even a normally exiting child and kills a child
        // whose socket died without answering the notebook shutdown RPC.
        let stopped = control(&mut self.guardian.lock().unwrap(), b'S');
        answer?;
        stopped.map(|_| ())
    }
}

/// Both endpoints are created before entering the private PID namespace. The
/// notebook's typed RPC socket still goes straight to the worker; only its
/// lifecycle commands pass through PID 1.
fn start_guardian(dir: &Path, startup: &Startup) -> Result<(UnixStream, UnixStream, bool)> {
    let (owner, notebook) = UnixStream::pair()?;
    let (worker, guardian) = UnixStream::pair()?;
    let notebook_fd = notebook.as_raw_fd();
    let guardian_fd = guardian.as_raw_fd();
    let data = senax_encoder::encode(startup)?;
    let mut command = Command::new("/proc/self/exe");
    command
        .arg("--notebook-guardian")
        .arg(base64::engine::general_purpose::STANDARD.encode(data))
        .arg(base64::engine::general_purpose::STANDARD.encode(dir.as_os_str().as_bytes()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWPID | libc::CLONE_NEWNS) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // unshare(CLONE_NEWPID) applies to the next fork. The intermediate
            // process exits; its child execs the same worker as namespace PID 1.
            let pid = libc::fork();
            if pid < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if pid > 0 {
                libc::_exit(0);
            }
            // Non-root exec would otherwise discard the capability needed to
            // make the mount tree private and mount namespace-local /proc.
            set_caps(CRIU_CAPS | (1 << 21), true)?;
            if libc::dup2(guardian_fd, 3) < 0
                || libc::dup2(notebook_fd, 4) < 0
                || libc::fcntl(3, libc::F_SETFD, 0) < 0
                || libc::fcntl(4, libc::F_SETFD, 0) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut bootstrap = command.spawn().context("start notebook guardian")?;
    drop(guardian);
    drop(notebook);
    let status = bootstrap.wait()?;
    if !status.success() {
        bail!("notebook guardian bootstrap exited: {status}");
    }
    // Failed namespace setup, a missing worker binary, or a failed restore
    // must not leave startup blocked on an orphaned guardian indefinitely.
    worker.set_read_timeout(Some(Duration::from_secs(25)))?;
    let restored = match read_reply(&worker) {
        Ok(reply) if reply == [0] => false,
        Ok(reply) if reply == [1] => true,
        Ok(reply) => bail!("invalid notebook guardian startup reply: {reply:?}"),
        Err(error) => return Err(error.context("notebook guardian startup")),
    };
    worker.set_read_timeout(None)?;
    Ok((owner, worker, restored))
}

fn control(stream: &mut UnixStream, operation: u8) -> Result<Vec<u8>> {
    stream.write_all(&[0, 0, 0, 1, operation])?;
    read_reply(stream)
}

fn read_reply(mut stream: impl Read) -> Result<Vec<u8>> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > 1024 * 1024 {
        bail!("invalid notebook guardian reply length: {size}");
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data)?;
    if data[0] != 0 {
        bail!("notebook guardian: {}", String::from_utf8_lossy(&data[1..]));
    }
    Ok(data[1..].to_vec())
}

fn reply(stream: &mut UnixStream, result: Result<Vec<u8>>) -> Result<()> {
    let data = match result {
        Ok(payload) => [&[0][..], payload.as_slice()].concat(),
        Err(error) => format!("\x01{error:#}").into_bytes(),
    };
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(&data)?;
    Ok(())
}

fn criu() -> PathBuf {
    std::env::var_os("RHO_CRIU")
        .map(PathBuf::from)
        .unwrap_or_else(|| "criu".into())
}

fn socket_identity(pid: u32) -> Result<String> {
    Ok(std::fs::read_link(format!("/proc/{pid}/fd/3"))?
        .to_string_lossy()
        .into_owned())
}

fn launch(startup: &Startup, socket: UnixStream) -> Result<Child> {
    let fd = socket.as_raw_fd();
    let data = senax_encoder::encode(startup)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--notebook-worker")
        .arg(base64::engine::general_purpose::STANDARD.encode(data))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The notebook must not inherit the guardian's CRIU permissions.
            set_caps(0, false)?;
            Ok(())
        });
    }
    command.spawn().context("launch notebook child")
}

// CAP_SYS_PTRACE and CAP_CHECKPOINT_RESTORE, granted inside the identity-
// mapped user namespace. CAP_SYS_ADMIN is used only to set up private /proc.
const CRIU_CAPS: u64 = (1 << 19) | (1 << 40);

fn set_caps(allowed: u64, ambient: bool) -> std::io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    unsafe {
        if libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        ) < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        for (index, item) in data.iter_mut().enumerate() {
            let mask = (allowed >> (index * 32)) as u32;
            if item.permitted & mask != mask {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "missing CRIU namespace capability",
                ));
            }
            item.effective = mask;
            item.permitted = mask;
            item.inheritable = if ambient { mask } else { 0 };
        }
        if libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if ambient {
            for cap in 0..64 {
                if allowed & (1 << cap) == 0 {
                    continue;
                }
                if libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_RAISE, cap, 0, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }
    }
    Ok(())
}

fn run_criu(mut command: Command, images: &Path, log: &str) -> Result<()> {
    unsafe {
        command.pre_exec(|| set_caps(CRIU_CAPS, true));
    }
    let output = command.output().context("starting CRIU")?;
    if !output.status.success() {
        let errors = std::fs::read_to_string(images.join(log))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("Error (") || line.contains("Warning ("))
            .take(12)
            .collect::<Vec<_>>()
            .join("; ");
        bail!(
            "CRIU {log} failed ({}): {} {errors}; see {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            images.join(log).display()
        );
    }
    Ok(())
}

fn dump(dir: &Path, pid: u32, child: &mut Option<Child>) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let name = uuid::Uuid::new_v4().to_string();
    let images = dir.join(&name);
    std::fs::create_dir(&images)?;
    let socket = socket_identity(pid)?;
    let inode = socket
        .strip_prefix("socket:[")
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| anyhow!("notebook fd 3 is not a Unix socket"))?;
    let mut command = Command::new(criu());
    command
        .arg("dump")
        .args(["--unprivileged", "-t"])
        .arg(pid.to_string())
        .arg("-D")
        .arg(&images)
        .args(["-o", "dump.log", "-v4", "--leave-stopped", "--external"])
        .arg(format!("unix[{inode}]"));
    run_criu(command, &images, "dump.log")?;
    // CRIU 4.2 can report success after failing to dump a mapped executable;
    // a checkpoint without the MM image crashes its restorer.
    if !images
        .join(format!("mm-{pid}.img"))
        .metadata()
        .is_ok_and(|image| image.len() > 0)
    {
        bail!(
            "CRIU dump omitted notebook memory image; see {}",
            images.join("dump.log").display()
        );
    }
    // Inspect the saved tree while CRIU leaves it stopped. The namespace
    // includes only this notebook and its descendants. A cursor such as
    // ns_last_pid can wrap, so calculate the actual maximum saved ID.
    let maximum = saved_id_bound()?;
    kill_namespace_children();
    reap(pid, child)?;
    reap_orphans();
    std::fs::write(
        dir.join("current.new"),
        format!("{name}\n{socket}\n{maximum}\n"),
    )?;
    std::fs::rename(dir.join("current.new"), dir.join("current"))?;
    Ok(())
}

/// Inspect IDs of every task and process group/session still present in this
/// private PID namespace after CRIU has stopped its target tree.
fn saved_id_bound() -> Result<u32> {
    let mut maximum = 1;
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        maximum = maximum.max(pid);
        let status = std::fs::read_to_string(entry.path().join("status"))?;
        for line in status.lines() {
            if line.starts_with("NSpgid:") || line.starts_with("NSsid:") {
                for value in line.split_whitespace().skip(1) {
                    maximum = maximum.max(value.parse()?);
                }
            }
        }
        for task in std::fs::read_dir(entry.path().join("task"))? {
            let task = task?;
            maximum = maximum.max(task.file_name().to_string_lossy().parse::<u32>()?);
        }
    }
    Ok(maximum)
}

fn kill_namespace_children() {
    // This PID namespace contains only guardian-owned notebook processes.
    // Kill the entire tree, including detached descendants of the notebook.
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() {
                if pid > 1 {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

fn reap_orphans() {
    loop {
        let mut status = 0;
        if unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } <= 0 {
            break;
        }
    }
}

fn restore(dir: &Path, replacement: &UnixStream) -> Result<u32> {
    // Consume the marker first: even a failed restore must not replay again.
    std::fs::rename(dir.join("current"), dir.join("used"))?;
    let snapshot = std::fs::read_to_string(dir.join("used"))?;
    let mut lines = snapshot.lines();
    let name = lines.next().ok_or_else(|| anyhow!("missing image name"))?;
    let socket = lines
        .next()
        .ok_or_else(|| anyhow!("missing socket identity"))?;
    let images = dir.join(name);
    let pidfile = dir.join("restored.pid");
    match std::fs::remove_file(&pidfile) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let maximum: u32 = lines
        .next()
        .ok_or_else(|| anyhow!("missing saved PID bound"))?
        .parse()?;
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")?
        .trim()
        .parse()?;
    // CRIU may start several helper processes before clone3(set_tid).
    // Decline restoration near wrap rather than reuse a saved ID.
    if maximum.saturating_add(1024) >= pid_max {
        bail!("saved notebook PID bound too close to pid_max");
    }
    std::fs::write("/proc/sys/kernel/ns_last_pid", maximum.to_string())?;
    let fd = replacement.as_raw_fd();
    let mut command = Command::new(criu());
    command
        .arg("restore")
        .args(["--unprivileged", "-d", "-D"])
        .arg(&images)
        .args(["-o", "restore.log", "-v4", "--pidfile"])
        .arg(&pidfile)
        .arg("--inherit-fd")
        .arg(format!("fd[4]:{socket}"));
    unsafe {
        command.pre_exec(move || {
            set_caps(CRIU_CAPS, true)?;
            if libc::dup2(fd, 4) < 0 || libc::fcntl(4, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Do not run_criu: pre_exec above also passes the external socket fd.
    let output = command.output().context("starting CRIU restore")?;
    if !output.status.success() {
        let errors = std::fs::read_to_string(images.join("restore.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("Error (") || line.contains("Warning ("))
            .take(12)
            .collect::<Vec<_>>()
            .join("; ");
        bail!(
            "CRIU restore failed ({}): {} {errors}; see {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            images.join("restore.log").display()
        );
    }
    let pid = std::fs::read_to_string(pidfile)?.trim().parse()?;
    if !socket_identity(pid)?.starts_with("socket:[") {
        bail!("restored notebook fd 3 is not a Unix socket");
    }
    Ok(pid)
}

fn reap(pid: u32, child: &mut Option<Child>) -> Result<()> {
    if let Some(mut spawned) = child.take() {
        spawned.wait()?;
    } else {
        let mut status = 0;
        if unsafe { libc::waitpid(pid as i32, &mut status, 0) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

fn kill_child(pid: Option<u32>, child: &mut Option<Child>) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let _ = reap(pid, child);
    }
}

/// Entrypoint dispatched before Tokio threads. The namespace PID 1 stays
/// alive to adopt restored children and coordinate snapshot lifecycle.
pub fn notebook_guardian_main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let data = base64::engine::general_purpose::STANDARD.decode(
        args.get(2)
            .ok_or_else(|| anyhow!("missing notebook startup"))?,
    )?;
    let mut data = bytes::Bytes::from(data);
    let startup: Startup = senax_encoder::decode(&mut data)?;
    if !data.is_empty() {
        bail!("trailing notebook startup data");
    }
    let dir = PathBuf::from(std::ffi::OsString::from_vec(
        base64::engine::general_purpose::STANDARD.decode(
            args.get(3)
                .ok_or_else(|| anyhow!("missing notebook snapshot directory"))?,
        )?,
    ));
    let mut control_socket = unsafe { UnixStream::from_raw_fd(3) };
    let mut notebook_socket = Some(unsafe { UnixStream::from_raw_fd(4) });
    for fd in [3, 4] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let setup = (|| -> Result<()> {
        if unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe {
            libc::mount(
                c"proc".as_ptr(),
                c"/proc".as_ptr(),
                c"proc".as_ptr(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        set_caps(CRIU_CAPS, false)?;
        Ok(())
    })();
    if let Err(error) = setup {
        reply(&mut control_socket, Err(error))?;
        return Ok(());
    }
    let mut child = None;
    let mut restored = false;
    let pid = if dir.join("current").exists() {
        match restore(&dir, notebook_socket.as_ref().unwrap()) {
            Ok(restored_pid) => {
                restored = true;
                restored_pid
            }
            Err(error) => {
                eprintln!("notebook restore failed; starting fresh: {error:#}");
                // A failed CRIU restore may leave a partial tree behind.
                kill_namespace_children();
                match launch(&startup, notebook_socket.take().unwrap()) {
                    Ok(spawned) => {
                        let pid = spawned.id();
                        child = Some(spawned);
                        pid
                    }
                    Err(error) => {
                        reply(&mut control_socket, Err(error))?;
                        return Ok(());
                    }
                }
            }
        }
    } else {
        match launch(&startup, notebook_socket.take().unwrap()) {
            Ok(spawned) => {
                let pid = spawned.id();
                child = Some(spawned);
                pid
            }
            Err(error) => {
                reply(&mut control_socket, Err(error))?;
                return Ok(());
            }
        }
    };
    // Close the guardian's duplicate so notebook RPC sees EOF if the child
    // fails or exits, rather than waiting for the guardian to exit.
    drop(notebook_socket);
    let mut pid = Some(pid);
    reply(&mut control_socket, Ok(vec![u8::from(restored)]))?;
    let mut dump_attempted = false;
    loop {
        let mut header = [0; 4];
        if control_socket.read_exact(&mut header).is_err() {
            break;
        }
        let length = u32::from_be_bytes(header);
        if length != 1 {
            break;
        }
        let mut request = [0; 1];
        if control_socket.read_exact(&mut request).is_err() {
            break;
        }
        match request[0] {
            b'C' => {
                dump_attempted = true;
                let result = dump(&dir, pid.unwrap(), &mut child);
                if result.is_ok() {
                    pid = None;
                }
                if reply(&mut control_socket, result.map(|()| Vec::new())).is_err() {
                    break;
                }
            }
            b'S' => {
                if dump_attempted || pid.is_some() {
                    kill_child(pid, &mut child);
                }
                // The worker may have closed the control socket while
                // shutting down; the notebook is already dead and reaped.
                let _ = reply(&mut control_socket, Ok(Vec::new()));
                return Ok(());
            }
            _ => break,
        }
    }
    kill_child(pid, &mut child);
    Ok(())
}

/// Entrypoint dispatched by the agent-host binary before its worker threads.
pub fn notebook_worker_main() -> Result<()> {
    let startup = std::env::args()
        .nth(2)
        .ok_or_else(|| anyhow!("missing notebook startup"))?;
    let data = base64::engine::general_purpose::STANDARD.decode(startup)?;
    let mut data = bytes::Bytes::from(data);
    let startup: Startup = senax_encoder::decode(&mut data)?;
    if !data.is_empty() {
        bail!("trailing notebook startup data");
    }
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow!("notebook TLS provider already installed"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let wake = Arc::new(Notify::new());
        let (mailroom, mut outbox) = Mailroom::new();
        let (service, service_rx) = ServiceClient::channel();
        let (shell, exports) = child_tools(&startup, &service, &mailroom);
        let notebook =
            Notebook::new(shell.clone(), exports, Arc::clone(&wake)).map_err(anyhow::Error::msg)?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(outbound) = outbox.recv().await {
                let event = match outbound {
                    Outbound::Send { cell, text } => Event::Send { cell, text },
                    Outbound::Status(text) => Event::Status(text),
                    Outbound::EndTurn => Event::EndTurn,
                    Outbound::Archive => Event::Archive,
                };
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        });
        let fresh = {
            let mailroom = Arc::clone(&mailroom);
            let wake = Arc::clone(&wake);
            let startup = startup.clone();
            let service = service.clone();
            Box::new(move || {
                let (shell, exports) = child_tools(&startup, &service, &mailroom);
                Notebook::new(shell, exports, Arc::clone(&wake))
            })
        };
        let socket = unsafe { UnixStream::from_raw_fd(3) };
        process::serve_with_services(
            notebook,
            socket,
            wake,
            Some(fresh),
            Some(event_rx),
            Some(service_rx),
        )
        .await
        .map_err(anyhow::Error::msg)
    })
}

fn child_tools(
    startup: &Startup,
    service: &ServiceClient,
    mailroom: &Arc<Mailroom>,
) -> (ShellTools, Vec<Export>) {
    let agent_host: AgentHost = Arc::new({
        let service = service.clone();
        move |call: SharedCall| {
            let service = service.clone();
            Box::pin(async move {
                let request = senax_encoder::encode(&call).map_err(|e| e.to_string())?;
                let reply = service
                    .request(ServiceKind::HostCall, request.to_vec())
                    .await?;
                let mut reply = bytes::Bytes::from(reply);
                let text: String = senax_encoder::decode(&mut reply).map_err(|e| e.to_string())?;
                if !reply.is_empty() {
                    return Err("trailing notebook host reply data".into());
                }
                Ok(text)
            })
        }
    });
    let credentials: CredentialsProvider = Arc::new({
        let service = service.clone();
        move || {
            let service = service.clone();
            Box::pin(async move {
                let reply = service
                    .request(ServiceKind::WebCredentials, Vec::new())
                    .await
                    .map_err(anyhow::Error::msg)?;
                let mut reply = bytes::Bytes::from(reply);
                let wire: CredentialsWire = senax_encoder::decode(&mut reply)?;
                if !reply.is_empty() {
                    bail!("trailing web credentials data");
                }
                Ok(Credentials {
                    bearer_token: wire.bearer_token,
                    account_id: wire.account_id,
                })
            })
        }
    });
    host_tools_with_services(
        &startup.cwd,
        startup.role,
        startup.id,
        Some(agent_host),
        startup.has_team,
        Some(credentials),
        Some(mailroom),
        true,
    )
}
