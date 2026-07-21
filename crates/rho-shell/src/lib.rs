//! Rho's persistent, editor-owned Brush shell kernel.
//!
//! Commands arrive over a close-on-exec sideband socket. One
//! `brush_core::Shell` retains cwd, variables, functions, aliases,
//! configuration, history, and jobs. Each execution receives a fresh
//! pseudoterminal whose output remains tagged with that execution; programs
//! that require a persistent controlling terminal belong in Rho's raw-terminal
//! surface.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use std::{env, thread};

use anyhow::{Context as _, anyhow};
use brush_builtins::{BuiltinSet, ShellBuilderExt as _};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{
    ExecutionControlFlow, ExecutionParameters, ProcessGroupPolicy, Shell, ShellValue,
    ShellVariable, SourceInfo,
};
use rand::RngCore as _;
use rand::rngs::OsRng;
use rho_shell_proto::{
    MAX_ACTIVE_PAGERS, MAX_PAGER_BYTES, MAX_PAGER_LINES, MAX_PROMPT_BYTES, PROTOCOL_VERSION,
    PagerAction, PagerMessage, PagerReply, Request, Response,
};

const RESPONSE_QUEUE: usize = 64;
const OUTPUT_CHUNK: usize = 16 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const PTY_ROWS: u16 = 24;
const PTY_COLS: u16 = 80;
const PAGER_SOCKET_ENV: &str = "RHO_PAGER_SOCKET";
const PAGER_TOKEN_ENV: &str = "RHO_PAGER_TOKEN";
const PAGER_EXECUTION_TOKEN_ENV: &str = "RHO_PAGER_EXECUTION_TOKEN";
const PAGER_PROTOCOL_VERSION: u16 = 1;
const PAGER_CHUNK: usize = 8 * 1024;

struct ActiveExecutionPty {
    execution: u64,
    controller: File,
    device: u64,
}

type ActivePty = Arc<Mutex<Option<ActiveExecutionPty>>>;
type PagerControls = Arc<Mutex<HashMap<(u64, u64, u64), UnixStream>>>;

#[derive(Clone)]
struct PagerControl {
    controls: PagerControls,
}

impl PagerControl {
    fn act(&self, execution: u64, pager: u64, page: u64, action: PagerAction) {
        if let Some(mut control) = self
            .controls
            .lock()
            .unwrap()
            .remove(&(execution, pager, page))
        {
            let _ = rho_shell_proto::write_pager_frame(&mut control, &PagerReply::from(action));
        }
    }
}

struct ExecutionPagerToken {
    token: String,
    executions: Arc<Mutex<HashMap<String, u64>>>,
}

impl Drop for ExecutionPagerToken {
    fn drop(&mut self) {
        self.executions.lock().unwrap().remove(&self.token);
    }
}

struct PagerServer {
    socket_path: PathBuf,
    shell_token: String,
    executions: Arc<Mutex<HashMap<String, u64>>>,
    control: PagerControl,
    stopping: Arc<AtomicBool>,
    listener: Option<thread::JoinHandle<()>>,
}

impl PagerServer {
    fn start(responses: mpsc::SyncSender<Response>) -> Option<Self> {
        let runtime = env::var_os("XDG_RUNTIME_DIR")?;
        let shell_token = random_token().ok()?;
        let socket_path =
            PathBuf::from(runtime).join(format!("rho-pager-{}.sock", &shell_token[..16]));
        let listener = UnixListener::bind(&socket_path).ok()?;
        let executions = Arc::new(Mutex::new(HashMap::new()));
        let controls = Arc::new(Mutex::new(HashMap::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let next_pager = Arc::new(AtomicU64::new(1));
        let active_connections = Arc::new(AtomicUsize::new(0));
        let control = PagerControl {
            controls: Arc::clone(&controls),
        };
        let listener_thread = {
            let executions = Arc::clone(&executions);
            let controls = Arc::clone(&controls);
            let stopping = Arc::clone(&stopping);
            let next_pager = Arc::clone(&next_pager);
            let active_connections = Arc::clone(&active_connections);
            let expected_token = shell_token.clone();
            thread::Builder::new()
                .name("rho-shell-pager-listener".into())
                .spawn(move || {
                    while !stopping.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                if stopping.load(Ordering::Acquire) {
                                    break;
                                }
                                if active_connections.fetch_add(1, Ordering::AcqRel)
                                    >= MAX_ACTIVE_PAGERS
                                {
                                    active_connections.fetch_sub(1, Ordering::AcqRel);
                                    continue;
                                }
                                let execution = Arc::clone(&executions);
                                let controls = Arc::clone(&controls);
                                let responses = responses.clone();
                                let expected_token = expected_token.clone();
                                let pager = next_pager.fetch_add(1, Ordering::Relaxed);
                                if pager == 0 {
                                    active_connections.fetch_sub(1, Ordering::AcqRel);
                                    continue;
                                }
                                let connection_count = Arc::clone(&active_connections);
                                if thread::Builder::new()
                                    .name(format!("rho-shell-pager-{pager}"))
                                    .spawn(move || {
                                        serve_pager(
                                            stream,
                                            pager,
                                            &expected_token,
                                            &execution,
                                            &controls,
                                            &responses,
                                        );
                                        connection_count.fetch_sub(1, Ordering::AcqRel);
                                    })
                                    .is_err()
                                {
                                    active_connections.fetch_sub(1, Ordering::AcqRel);
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                })
        };
        let listener_thread = match listener_thread {
            Ok(listener) => listener,
            Err(_) => {
                let _ = std::fs::remove_file(&socket_path);
                return None;
            }
        };
        Some(Self {
            socket_path,
            shell_token,
            executions,
            control,
            stopping,
            listener: Some(listener_thread),
        })
    }

    fn configure_shell(&self, shell: &mut Shell) -> anyhow::Result<()> {
        set_exported(
            shell,
            PAGER_SOCKET_ENV,
            self.socket_path.to_string_lossy().into_owned(),
        )?;
        set_exported(shell, PAGER_TOKEN_ENV, self.shell_token.clone())
    }

    fn begin_execution(
        &self,
        shell: &mut Shell,
        execution: u64,
    ) -> anyhow::Result<ExecutionPagerToken> {
        let token = random_token().context("generate pager execution token")?;
        set_exported(shell, PAGER_EXECUTION_TOKEN_ENV, token.clone())?;
        self.executions
            .lock()
            .unwrap()
            .insert(token.clone(), execution);
        Ok(ExecutionPagerToken {
            token,
            executions: Arc::clone(&self.executions),
        })
    }
}

impl Drop for PagerServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn serve_pager(
    mut stream: UnixStream,
    pager: u64,
    expected_token: &str,
    executions: &Mutex<HashMap<String, u64>>,
    controls: &Mutex<HashMap<(u64, u64, u64), UnixStream>>,
    responses: &mpsc::SyncSender<Response>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let Ok(PagerMessage::Hello {
        protocol,
        token,
        execution_token,
    }) = rho_shell_proto::read_pager_frame(&mut stream)
    else {
        return;
    };
    if protocol != PAGER_PROTOCOL_VERSION || token != expected_token {
        return;
    }
    let Some(execution) = executions.lock().unwrap().get(&execution_token).copied() else {
        return;
    };
    let _ = stream.set_read_timeout(None);
    if responses
        .send(Response::PagerStarted { execution, pager })
        .is_err()
    {
        return;
    }
    let mut last_page = 0;
    while let Ok(message) = rho_shell_proto::read_pager_frame(&mut stream) {
        let PagerMessage::Paused { page, lines, bytes } = message else {
            break;
        };
        if page <= last_page {
            break;
        }
        last_page = page;
        let Ok((mut actions_rx, actions_tx)) = UnixStream::pair() else {
            break;
        };
        controls
            .lock()
            .unwrap()
            .insert((execution, pager, page), actions_tx);
        if responses
            .send(Response::PagerPaused {
                execution,
                pager,
                page,
                lines,
                bytes,
            })
            .is_err()
        {
            break;
        }
        let Some(action) = wait_for_pager_action(&stream, &mut actions_rx) else {
            break;
        };
        if rho_shell_proto::write_pager_frame(&mut stream, &action).is_err() {
            break;
        }
        if responses
            .send(Response::PagerResumed { execution, pager })
            .is_err()
        {
            break;
        }
    }
    controls
        .lock()
        .unwrap()
        .retain(|(item_execution, item_pager, _), _| {
            *item_execution != execution || *item_pager != pager
        });
    let _ = responses.send(Response::PagerFinished { execution, pager });
}

fn wait_for_pager_action(stream: &UnixStream, actions: &mut UnixStream) -> Option<PagerReply> {
    let mut descriptors = [
        libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: actions.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if descriptors[0].revents != 0 {
            return None;
        }
        if descriptors[1].revents != 0 {
            return rho_shell_proto::read_pager_frame(actions).ok();
        }
    }
}

fn set_exported(shell: &mut Shell, name: &str, value: impl Into<ShellValue>) -> anyhow::Result<()> {
    let mut variable = ShellVariable::new(value);
    variable.export();
    shell
        .set_env_global(name, variable)
        .map_err(|error| anyhow!(error))
        .with_context(|| format!("set {name}"))
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

impl ActiveExecutionPty {
    fn send_eof(&mut self) {
        use rustix_openpty::rustix::termios::SpecialCodeIndex;

        let eof = rustix_openpty::rustix::termios::tcgetattr(&self.controller)
            .map(|termios| termios.special_codes[SpecialCodeIndex::VEOF])
            .unwrap_or(0x04);
        let _ = self.controller.write_all(&[eof]);
    }
}

pub async fn run() -> anyhow::Result<()> {
    let control = Arc::new(take_control_stdin()?);
    let control_writer = Arc::clone(&control);
    replace_standard_streams().context("detach shell host standard streams")?;

    let (responses_tx, responses_rx) = mpsc::sync_channel(RESPONSE_QUEUE);
    let pager_server = PagerServer::start(responses_tx.clone());
    let response_writer = thread::Builder::new()
        .name("rho-shell-protocol".into())
        .spawn(move || {
            let mut writer = control_writer.as_ref();
            while let Ok(response) = responses_rx.recv() {
                let exiting = matches!(response, Response::Exited { .. });
                if rho_shell_proto::write_frame(&mut writer, &response).is_err() || exiting {
                    break;
                }
            }
        })
        .context("spawn protocol writer")?;

    let active_pty: ActivePty = Arc::new(Mutex::new(None));
    let (requests_tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let control = Arc::clone(&control);
        let active_pty = Arc::clone(&active_pty);
        let responses = responses_tx.clone();
        let pager = pager_server.as_ref().map(|pager| pager.control.clone());
        thread::Builder::new()
            .name("rho-shell-control".into())
            .spawn(move || {
                read_requests(
                    control.as_ref(),
                    requests_tx,
                    &active_pty,
                    pager.as_ref(),
                    &responses,
                );
            })
            .context("spawn protocol reader")?;
    }

    let mut shell = initialize_shell().await?;
    if let Some(pager) = &pager_server {
        pager.configure_shell(&mut shell)?;
    }
    let status = run_kernel(
        &mut shell,
        requests_rx,
        &responses_tx,
        &active_pty,
        pager_server.as_ref(),
    )
    .await;

    let _ = shell.on_exit().await;
    let _ = shell.save_history();
    let _ = responses_tx.send(Response::Exited { status });
    drop(responses_tx);
    let _ = response_writer.join();
    drop(pager_server);
    std::process::exit(status.clamp(0, u8::MAX.into()));
}

fn take_control_stdin() -> anyhow::Result<File> {
    // The daemon passes the full-duplex protocol socket as stdin. Keep a
    // close-on-exec duplicate before replacing process stdio with /dev/null.
    let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("duplicate shell control stdin");
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn replace_standard_streams() -> std::io::Result<()> {
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(null.as_raw_fd(), target) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn read_requests(
    mut control: &File,
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    active_pty: &Mutex<Option<ActiveExecutionPty>>,
    pager: Option<&PagerControl>,
    responses: &mpsc::SyncSender<Response>,
) {
    loop {
        let request = match rho_shell_proto::read_frame::<Request>(&mut control) {
            Ok(request) => request,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                let _ = responses.send(Response::Error {
                    execution: None,
                    message: format!("read shell request: {error}"),
                });
                break;
            }
        };
        match request {
            Request::Execute { .. } => {
                if requests.send(request).is_err() {
                    break;
                }
            }
            Request::Interrupt { execution } => {
                let device = active_pty
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|active| active.execution == execution)
                    .map(|active| active.device);
                if let Some(device) = device {
                    interrupt_execution(device);
                }
            }
            Request::Eof { execution } => {
                if let Some(active) = active_pty
                    .lock()
                    .unwrap()
                    .as_mut()
                    .filter(|active| active.execution == execution)
                {
                    active.send_eof();
                }
            }
            Request::PagerAction {
                execution,
                pager: pager_id,
                page,
                action,
            } => {
                if let Some(pager) = pager {
                    pager.act(execution, pager_id, page, action);
                }
            }
            Request::Shutdown => {
                let _ = requests.send(Request::Shutdown);
                break;
            }
        }
    }
}

fn interrupt_execution(device: u64) {
    // The evaluator outlives every execution PTY, so its process group cannot
    // identify the active command. Match session descendants by the PTY device
    // still connected to one of their standard descriptors instead.
    let shell_pid = unsafe { libc::getpid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for pid in entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid != shell_pid && unsafe { libc::getsid(*pid) } == shell_pid)
        .filter(|pid| {
            [0, 1, 2].into_iter().any(|fd| {
                std::fs::metadata(format!("/proc/{pid}/fd/{fd}"))
                    .is_ok_and(|metadata| metadata.rdev() == device)
            })
        })
    {
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
    }
}

async fn initialize_shell() -> anyhow::Result<Shell> {
    let fds = HashMap::from([
        (OpenFiles::STDIN_FD, brush_core::openfiles::null()?),
        (OpenFiles::STDOUT_FD, brush_core::openfiles::null()?),
        (OpenFiles::STDERR_FD, brush_core::openfiles::null()?),
    ]);
    let mut shell = Shell::builder()
        .interactive(true)
        .no_editing(true)
        .disable_option("monitor")
        .fds(fds)
        .default_builtins(BuiltinSet::BashMode)
        .build()
        .await
        .map_err(|error| anyhow!(error))
        .context("initialize Brush shell")?;
    // Brush's stock prompt is `\s-\v\$ `. The embedded shell has no argv[0]
    // shell name, so that renders as the unhelpful `-<version>$ `. Keep any
    // prompt supplied by startup files, but use a conventional prompt when
    // the stock value is still present.
    if shell.env_str("PS1").as_deref() == Some(r"\s-\v\$ ") {
        shell
            .set_env_global("PS1", ShellVariable::new(r"\$ "))
            .context("set default prompt")?;
    }
    Ok(shell)
}

async fn run_kernel(
    shell: &mut Shell,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<Request>,
    responses: &mpsc::SyncSender<Response>,
    active_pty: &ActivePty,
    pager_server: Option<&PagerServer>,
) -> i32 {
    let mut current_prompt = prepare_prompt(shell).await;
    let mut current_cwd = cwd(shell);
    if responses
        .send(Response::Ready {
            protocol: PROTOCOL_VERSION,
            prompt: current_prompt.clone(),
            cwd: current_cwd.clone(),
        })
        .is_err()
    {
        return 0;
    }

    loop {
        let request = match requests.recv().await {
            Some(request) => request,
            None => return 0,
        };
        let Request::Execute { execution, command } = request else {
            return 0;
        };
        if !rho_shell_proto::command_fits(&command) {
            let _ = responses.send(Response::Error {
                execution: Some(execution),
                message: "command exceeds the shell input limit".into(),
            });
            continue;
        }
        let command = command.trim_end_matches(['\r', '\n']);
        let execution_token = match pager_server
            .map(|pager| pager.begin_execution(shell, execution))
            .transpose()
        {
            Ok(token) => token,
            Err(error) => {
                let _ = responses.send(Response::Error {
                    execution: Some(execution),
                    message: format!("prepare pager: {error}"),
                });
                continue;
            }
        };
        let (params, execution_pty, output_done) =
            match command_io(execution, responses.clone(), execution_token) {
                Ok(capture) => capture,
                Err(error) => {
                    let _ = responses.send(Response::Error {
                        execution: Some(execution),
                        message: format!("prepare command IO: {error}"),
                    });
                    continue;
                }
            };
        *active_pty.lock().unwrap() = Some(execution_pty);
        if responses.send(Response::Started { execution }).is_err() {
            active_pty.lock().unwrap().take();
            return 0;
        }

        let mut status = shell.last_exit_status().into();
        let mut exiting = false;
        if !command.is_empty() {
            let _ = shell.add_to_history(command);
            match shell
                .run_string(command, &SourceInfo::from("rho"), &params)
                .await
            {
                Ok(result) => {
                    status = u8::from(result.exit_code).into();
                    exiting = matches!(result.next_control_flow, ExecutionControlFlow::ExitShell);
                }
                Err(error) => {
                    status = 2;
                    shell.set_last_exit_status(status as u8);
                    let _ = responses.send(Response::Output {
                        execution,
                        data: format!("brush: {error}\n").into_bytes(),
                    });
                }
            }
            shell.increment_interactive_line_offset(command.lines().count().max(1));
        }
        drop(params);
        let _ = output_done.recv_timeout(OUTPUT_DRAIN_GRACE);
        {
            let mut active = active_pty.lock().unwrap();
            if active
                .as_ref()
                .is_some_and(|active| active.execution == execution)
            {
                active.take();
            }
        }

        current_cwd = cwd(shell);
        current_prompt = if exiting {
            String::new()
        } else {
            prepare_prompt(shell).await
        };
        if responses
            .send(Response::Finished {
                execution,
                status,
                prompt: current_prompt.clone(),
                cwd: current_cwd.clone(),
            })
            .is_err()
        {
            return status;
        }
        if exiting {
            return status;
        }
    }
}

fn command_io(
    execution: u64,
    responses: mpsc::SyncSender<Response>,
    pager_token: Option<ExecutionPagerToken>,
) -> anyhow::Result<(ExecutionParameters, ActiveExecutionPty, mpsc::Receiver<()>)> {
    let size = rustix_openpty::rustix::termios::Winsize {
        ws_row: PTY_ROWS,
        ws_col: PTY_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = rustix_openpty::openpty(None, Some(&size)).context("open execution pty")?;
    let controller = File::from(pty.controller);
    let slave = File::from(pty.user);
    let device = slave.metadata().context("identify execution pty")?.rdev();
    let control = controller
        .try_clone()
        .context("duplicate execution pty controller")?;

    let mut params = ExecutionParameters::default();
    params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;
    params.set_fd(
        OpenFiles::STDIN_FD,
        OpenFile::from(slave.try_clone().context("duplicate execution pty stdin")?),
    );
    params.set_fd(
        OpenFiles::STDOUT_FD,
        OpenFile::from(
            slave
                .try_clone()
                .context("duplicate execution pty stdout")?,
        ),
    );
    params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(slave));

    let (done_tx, done_rx) = mpsc::channel();
    thread::Builder::new()
        .name(format!("rho-shell-output-{execution}"))
        .spawn(move || relay_output(controller, execution, responses, done_tx, pager_token))
        .context("spawn pty output relay")?;
    Ok((
        params,
        ActiveExecutionPty {
            execution,
            controller: control,
            device,
        },
        done_rx,
    ))
}

fn relay_output(
    mut controller: File,
    execution: u64,
    responses: mpsc::SyncSender<Response>,
    done: mpsc::Sender<()>,
    pager_token: Option<ExecutionPagerToken>,
) {
    let mut buffer = vec![0; OUTPUT_CHUNK];
    loop {
        match controller.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if responses
                    .send(Response::Output {
                        execution,
                        data: buffer[..read].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(_) => break,
        }
    }
    drop(pager_token);
    let _ = done.send(());
}

/// Runs the pager helper selected through `PAGER`/`GIT_PAGER`.
///
/// Output remains ordinary stdout. The private socket carries only page-credit
/// control, and any missing or lost control connection degrades to `cat`.
pub fn run_pager() -> anyhow::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut control = connect_pager_control();
    relay_paged(&mut input, &mut output, control.as_mut()).context("relay pager output")
}

fn connect_pager_control() -> Option<UnixStream> {
    let socket = env::var_os(PAGER_SOCKET_ENV)?;
    let token = env::var(PAGER_TOKEN_ENV).ok()?;
    let execution_token = env::var(PAGER_EXECUTION_TOKEN_ENV).ok()?;
    let mut stream = UnixStream::connect(socket).ok()?;
    rho_shell_proto::write_pager_frame(
        &mut stream,
        &PagerMessage::Hello {
            protocol: PAGER_PROTOCOL_VERSION,
            token,
            execution_token,
        },
    )
    .ok()?;
    Some(stream)
}

fn relay_paged(
    input: &mut impl io::Read,
    output: &mut impl io::Write,
    control: Option<&mut UnixStream>,
) -> io::Result<()> {
    let page_lines = env::var("LINES")
        .ok()
        .and_then(|lines| lines.parse::<usize>().ok())
        .unwrap_or(PTY_ROWS.into())
        .clamp(1, MAX_PAGER_LINES as usize);
    relay_paged_with_lines(input, output, control, page_lines)
}

fn relay_paged_with_lines(
    input: &mut impl io::Read,
    output: &mut impl io::Write,
    mut control: Option<&mut UnixStream>,
    page_lines: usize,
) -> io::Result<()> {
    if control.is_none() {
        return io::copy(input, output).map(|_| ());
    }
    let mut pending = Vec::new();
    let mut pending_start = 0;
    let mut page = 1_u64;
    let mut lines = 0_usize;
    let mut bytes = 0_usize;

    loop {
        if pending_start == pending.len() {
            pending.resize(PAGER_CHUNK, 0);
            let read = input.read(&mut pending)?;
            if read == 0 {
                return output.flush();
            }
            pending.truncate(read);
            pending_start = 0;
        }

        let byte_credit = MAX_PAGER_BYTES as usize - bytes;
        let line_credit = page_lines - lines;
        let available = &pending[pending_start..];
        let mut take = available.len().min(byte_credit);
        let mut added_lines = 0;
        for (index, byte) in available[..take].iter().enumerate() {
            if *byte == b'\n' {
                added_lines += 1;
                if added_lines == line_credit {
                    take = index + 1;
                    break;
                }
            }
        }
        output.write_all(&available[..take])?;
        output.flush()?;
        pending_start += take;
        bytes += take;
        lines += added_lines;

        if bytes < MAX_PAGER_BYTES as usize && lines < page_lines {
            continue;
        }

        let Some(stream) = control.as_deref_mut() else {
            output.write_all(&pending[pending_start..])?;
            return io::copy(input, output).map(|_| ());
        };
        let paused = PagerMessage::Paused {
            page,
            lines: lines as u32,
            bytes: bytes as u64,
        };
        let action = rho_shell_proto::write_pager_frame(stream, &paused)
            .and_then(|()| rho_shell_proto::read_pager_frame::<PagerReply>(stream));
        match action {
            Ok(PagerReply::Continue) => {
                page = page.saturating_add(1);
                lines = 0;
                bytes = 0;
            }
            Ok(PagerReply::Drain) | Err(_) => {
                output.write_all(&pending[pending_start..])?;
                return io::copy(input, output).map(|_| ());
            }
            Ok(PagerReply::Quit) => return Ok(()),
        }
    }
}

async fn prepare_prompt(shell: &mut Shell) -> String {
    let _ = shell.check_for_completed_jobs();
    let _ = run_prompt_command(shell).await;
    let mut prompt = shell
        .compose_prompt()
        .await
        .unwrap_or_else(|_| "brush$ ".into());
    if prompt.len() > MAX_PROMPT_BYTES {
        let mut end = MAX_PROMPT_BYTES;
        while !prompt.is_char_boundary(end) {
            end -= 1;
        }
        prompt.truncate(end);
    }
    if prompt.is_empty() {
        "> ".into()
    } else {
        prompt
    }
}

async fn run_prompt_command(shell: &mut Shell) -> Result<(), brush_core::Error> {
    let commands = match shell
        .env_var("PROMPT_COMMAND")
        .map(|var| var.value().clone())
    {
        Some(ShellValue::String(command)) => vec![command],
        Some(ShellValue::IndexedArray(values)) => values.values().cloned().collect(),
        _ => return Ok(()),
    };
    let previous_status = shell.last_exit_status();
    let previous_pipeline = shell.last_pipeline_statuses().to_vec();
    let params = shell.default_exec_params();
    let mut result = Ok(());
    for command in commands {
        if let Err(error) = shell
            .run_string(command, &SourceInfo::from("PROMPT_COMMAND"), &params)
            .await
        {
            result = Err(error);
            break;
        }
    }
    *shell.last_pipeline_statuses_mut() = previous_pipeline;
    shell.set_last_exit_status(previous_status);
    result
}

fn cwd(shell: &Shell) -> String {
    shell.working_dir().to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pager_waits_for_credit_after_each_page() {
        let (mut pager, mut shell) = UnixStream::pair().unwrap();
        let controller = thread::spawn(move || {
            assert_eq!(
                rho_shell_proto::read_pager_frame::<PagerMessage>(&mut shell).unwrap(),
                PagerMessage::Paused {
                    page: 1,
                    lines: 2,
                    bytes: 4,
                }
            );
            rho_shell_proto::write_pager_frame(&mut shell, &PagerReply::Continue).unwrap();
        });
        let mut input = io::Cursor::new(b"a\nb\nc\n".to_vec());
        let mut output = Vec::new();
        relay_paged_with_lines(&mut input, &mut output, Some(&mut pager), 2).unwrap();
        controller.join().unwrap();
        assert_eq!(output, b"a\nb\nc\n");
    }

    #[test]
    fn quitting_a_page_stops_reading_the_producer() {
        let (mut pager, mut shell) = UnixStream::pair().unwrap();
        let controller = thread::spawn(move || {
            let _: PagerMessage = rho_shell_proto::read_pager_frame(&mut shell).unwrap();
            rho_shell_proto::write_pager_frame(&mut shell, &PagerReply::Quit).unwrap();
        });
        let source = b"a\n".repeat(PAGER_CHUNK);
        let mut input = io::Cursor::new(source);
        let mut output = Vec::new();
        relay_paged_with_lines(&mut input, &mut output, Some(&mut pager), 2).unwrap();
        controller.join().unwrap();
        assert_eq!(output, b"a\na\n");
        assert_eq!(input.position(), PAGER_CHUNK as u64);
        assert!(input.position() < input.get_ref().len() as u64);
    }

    #[test]
    fn losing_pager_control_fails_open() {
        let (mut pager, shell) = UnixStream::pair().unwrap();
        drop(shell);
        let source = b"a\n".repeat(PAGER_CHUNK);
        let mut input = io::Cursor::new(source.clone());
        let mut output = Vec::new();
        relay_paged_with_lines(&mut input, &mut output, Some(&mut pager), 2).unwrap();
        assert_eq!(output, source);
    }
}
