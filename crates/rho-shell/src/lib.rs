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
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::fs::MetadataExt as _;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use brush_builtins::{BuiltinSet, ShellBuilderExt as _};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{
    ExecutionControlFlow, ExecutionParameters, ProcessGroupPolicy, Shell, ShellValue, SourceInfo,
};
use rho_shell_proto::{MAX_PROMPT_BYTES, PROTOCOL_VERSION, Request, Response};

const RESPONSE_QUEUE: usize = 64;
const OUTPUT_CHUNK: usize = 16 * 1024;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const PTY_ROWS: u16 = 24;
const PTY_COLS: u16 = 80;

struct ActiveExecutionPty {
    execution: u64,
    controller: File,
    device: u64,
}

type ActivePty = Arc<Mutex<Option<ActiveExecutionPty>>>;

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
        thread::Builder::new()
            .name("rho-shell-control".into())
            .spawn(move || read_requests(control.as_ref(), requests_tx, &active_pty, &responses))
            .context("spawn protocol reader")?;
    }

    let mut shell = initialize_shell().await?;
    let status = run_kernel(&mut shell, requests_rx, &responses_tx, &active_pty).await;

    let _ = shell.on_exit().await;
    let _ = shell.save_history();
    let _ = responses_tx.send(Response::Exited { status });
    drop(responses_tx);
    let _ = response_writer.join();
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
    Shell::builder()
        .interactive(true)
        .no_editing(true)
        .disable_option("monitor")
        .fds(fds)
        .default_builtins(BuiltinSet::BashMode)
        .build()
        .await
        .map_err(|error| anyhow!(error))
        .context("initialize Brush shell")
}

async fn run_kernel(
    shell: &mut Shell,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<Request>,
    responses: &mpsc::SyncSender<Response>,
    active_pty: &ActivePty,
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
        let (params, execution_pty, output_done) = match command_io(execution, responses.clone()) {
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
        .spawn(move || relay_output(controller, execution, responses, done_tx))
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
    let _ = done.send(());
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
