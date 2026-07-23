use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::time::Duration;

use rho_shell_proto::{PROTOCOL_VERSION, Request, Response};

#[test]
fn output_events_keep_execution_boundaries_and_merge_standard_streams() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let (mut control, child_control) = std::os::unix::net::UnixStream::pair().unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_rho-shell"));
    command
        .current_dir(temp.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &home)
        .env("USER", "rho-test")
        .env("LOGNAME", "rho-test")
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .env("XDG_RUNTIME_DIR", temp.path())
        .env("PAGER", env!("CARGO_BIN_EXE_rho-pager"))
        .env("GIT_PAGER", env!("CARGO_BIN_EXE_rho-pager"))
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(child_control)))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure only invokes async-signal-safe descriptor/session
    // setup before exec in this single-threaded child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();

    let Response::Ready {
        protocol, prompt, ..
    } = read(&mut control)
    else {
        panic!("rho-shell did not send Ready");
    };
    assert_eq!(protocol, PROTOCOL_VERSION);
    assert!(matches!(prompt.as_str(), "$ " | "# "), "{prompt:?}");

    write(
        &mut control,
        &Request::Execute {
            execution: 10,
            command: concat!(
                "command sh -c '",
                "stdin=$(readlink /proc/$$/fd/0); ",
                "test -t 0 && test -t 1 && test -t 2 && ",
                "test \"$stdin\" = \"$(readlink /proc/$$/fd/1)\" && ",
                "test \"$stdin\" = \"$(readlink /proc/$$/fd/2)\" && printf pty'"
            )
            .into(),
        },
    );
    let mut terminal = Vec::new();
    loop {
        match read(&mut control) {
            Response::Started { execution: 10 } => {}
            Response::Output {
                execution: 10,
                data,
            } => terminal.extend(data),
            Response::Finished { execution: 10, .. } => break,
            frame => panic!("unexpected frame: {frame:?}"),
        }
    }
    assert_eq!(terminal, b"pty");

    write(
        &mut control,
        &Request::Execute {
            execution: 11,
            command: "printf out; printf err >&2".into(),
        },
    );
    let mut output = Vec::new();
    loop {
        match read(&mut control) {
            Response::Started { execution, .. } => assert_eq!(execution, 11),
            Response::Output { execution, data } => {
                assert_eq!(execution, 11);
                output.extend(data);
            }
            Response::Finished { execution, .. } => {
                assert_eq!(execution, 11);
                break;
            }
            frame => panic!("unexpected frame: {frame:?}"),
        }
    }
    assert_eq!(output, b"outerr");

    write(
        &mut control,
        &Request::Execute {
            execution: 12,
            command: "rho_value=persisted; rho_fn() { printf '%s' \"$rho_value\"; }".into(),
        },
    );
    while !matches!(read(&mut control), Response::Finished { execution: 12, .. }) {}
    write(
        &mut control,
        &Request::Execute {
            execution: 13,
            command: "rho_fn".into(),
        },
    );
    let mut persisted = Vec::new();
    loop {
        match read(&mut control) {
            Response::Output {
                execution: 13,
                data,
            } => persisted.extend(data),
            Response::Finished { execution: 13, .. } => break,
            Response::Started { execution: 13, .. } => {}
            frame => panic!("unexpected frame: {frame:?}"),
        }
    }
    assert_eq!(persisted, b"persisted");

    write(
        &mut control,
        &Request::Execute {
            execution: 14,
            command: "command sh -c 'readlink /proc/self/fd/0; sleep 0.5; printf late' &".into(),
        },
    );
    let mut first_pty = Vec::new();
    let mut second_pty = Vec::new();
    let mut first_finished = false;
    let mut second_finished = false;
    loop {
        match read(&mut control) {
            Response::Started { execution } => assert!(matches!(execution, 14 | 15)),
            Response::Finished { execution: 14, .. } => {
                first_finished = true;
                write(
                    &mut control,
                    &Request::Execute {
                        execution: 15,
                        command: "readlink /proc/self/fd/0".into(),
                    },
                );
            }
            Response::Finished { execution: 15, .. } => second_finished = true,
            Response::Output {
                execution: 14,
                data,
            } => first_pty.extend(data),
            Response::Output {
                execution: 15,
                data,
            } => second_pty.extend(data),
            frame => panic!("unexpected frame: {frame:?}"),
        }
        if first_finished && second_finished && first_pty.ends_with(b"late") {
            break;
        }
    }
    let first_device = String::from_utf8(first_pty).unwrap();
    let first_device = first_device.lines().next().unwrap().trim_end_matches('\r');
    let second_device = String::from_utf8(second_pty).unwrap();
    let second_device = second_device.trim().trim_end_matches('\r');
    assert_ne!(first_device, second_device, "executions reused a live PTY");

    write(
        &mut control,
        &Request::Execute {
            execution: 16,
            command: r#"seq 1 50 | "$PAGER""#.into(),
        },
    );
    let mut paged = Vec::new();
    let mut started_pager = None;
    let pager = loop {
        match read(&mut control) {
            Response::Started { execution: 16 } => {}
            Response::PagerStarted {
                execution: 16,
                pager,
            } => started_pager = Some(pager),
            Response::Output {
                execution: 16,
                data,
            } => paged.extend(data),
            Response::PagerPaused {
                execution: 16,
                pager,
                page: 1,
                lines: 24,
                ..
            } => {
                assert_eq!(started_pager, Some(pager));
                break pager;
            }
            frame => panic!("unexpected frame before pager pause: {frame:?}"),
        }
    };
    // Pager control and PTY output use independent relay threads. The pager
    // flushes the page before announcing the pause, but the control frame can
    // still reach the protocol writer before the PTY relay's output frame.
    while !paged.ends_with(b"24\r\n") {
        match read(&mut control) {
            Response::Output {
                execution: 16,
                data,
            } => paged.extend(data),
            frame => panic!("unexpected frame while draining paused page: {frame:?}"),
        }
    }
    assert!(paged.ends_with(b"24\r\n"), "{paged:?}");
    write(
        &mut control,
        &Request::PagerAction {
            execution: 16,
            pager,
            page: 1,
            action: rho_shell_proto::PagerAction::Continue,
        },
    );
    // A duplicate credit for page 1 must not apply to a later page.
    write(
        &mut control,
        &Request::PagerAction {
            execution: 16,
            pager,
            page: 1,
            action: rho_shell_proto::PagerAction::Continue,
        },
    );
    let mut resumed = 0;
    loop {
        match read(&mut control) {
            Response::PagerResumed {
                execution: 16,
                pager: resumed_pager,
            } => {
                assert_eq!(resumed_pager, pager);
                resumed += 1;
            }
            Response::Output {
                execution: 16,
                data,
            } => paged.extend(data),
            Response::PagerPaused {
                execution: 16,
                pager: paused_pager,
                page: 2,
                ..
            } => {
                assert_eq!(paused_pager, pager);
                break;
            }
            frame => panic!("unexpected frame after pager resume: {frame:?}"),
        }
    }
    assert_eq!(resumed, 1);
    control
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    loop {
        match rho_shell_proto::read_frame(&mut control) {
            Ok(Response::Output {
                execution: 16,
                data,
            }) => paged.extend(data),
            Ok(frame) => panic!("stale page credit produced a frame: {frame:?}"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("read after second pager pause: {error}"),
        }
    }
    control
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write(
        &mut control,
        &Request::PagerAction {
            execution: 16,
            pager,
            page: 2,
            action: rho_shell_proto::PagerAction::Continue,
        },
    );
    let mut pager_finished = false;
    let mut execution_finished = false;
    while !(pager_finished && execution_finished && paged.ends_with(b"50\r\n")) {
        match read(&mut control) {
            Response::PagerResumed {
                execution: 16,
                pager: resumed_pager,
            } => {
                assert_eq!(resumed_pager, pager);
                resumed += 1;
            }
            Response::PagerFinished {
                execution: 16,
                pager: finished_pager,
            } => {
                assert_eq!(finished_pager, pager);
                pager_finished = true;
            }
            Response::Output {
                execution: 16,
                data,
            } => paged.extend(data),
            Response::Finished { execution: 16, .. } => execution_finished = true,
            frame => panic!("unexpected frame after second pager resume: {frame:?}"),
        }
    }
    assert_eq!(resumed, 2);
    assert!(paged.ends_with(b"50\r\n"), "{paged:?}");

    write(
        &mut control,
        &Request::Execute {
            execution: 17,
            command: r#"printf short | "$PAGER""#.into(),
        },
    );
    let mut short = Vec::new();
    let mut short_pager = None;
    let mut short_finished = false;
    let mut short_execution_finished = false;
    while !(short_finished && short_execution_finished && short == b"short") {
        match read(&mut control) {
            Response::Started { execution: 17 } => {}
            Response::PagerStarted {
                execution: 17,
                pager,
            } => short_pager = Some(pager),
            Response::PagerFinished {
                execution: 17,
                pager,
            } => {
                assert_eq!(short_pager, Some(pager));
                short_finished = true;
            }
            Response::Output {
                execution: 17,
                data,
            } => short.extend(data),
            Response::Finished { execution: 17, .. } => short_execution_finished = true,
            frame => panic!("unexpected frame from short pager: {frame:?}"),
        }
    }
    assert!(short_finished);
    assert_eq!(short, b"short");

    write(
        &mut control,
        &Request::Execute {
            execution: 18,
            command: r#"seq 1 30 | "$PAGER""#.into(),
        },
    );
    let interrupted_pager = loop {
        match read(&mut control) {
            Response::Started { execution: 18 }
            | Response::PagerStarted { execution: 18, .. }
            | Response::Output { execution: 18, .. } => {}
            Response::PagerPaused {
                execution: 18,
                pager,
                page: 1,
                ..
            } => break pager,
            frame => panic!("unexpected frame before interrupted pager pause: {frame:?}"),
        }
    };
    write(&mut control, &Request::Interrupt { execution: 18 });
    let mut interrupted_pager_finished = false;
    let mut interrupted_execution_finished = false;
    while !(interrupted_pager_finished && interrupted_execution_finished) {
        match read(&mut control) {
            Response::PagerFinished {
                execution: 18,
                pager,
            } => {
                assert_eq!(pager, interrupted_pager);
                interrupted_pager_finished = true;
            }
            Response::Output { execution: 18, .. } => {}
            Response::Finished { execution: 18, .. } => interrupted_execution_finished = true,
            frame => panic!("unexpected frame after interrupted pager: {frame:?}"),
        }
    }
    assert!(interrupted_pager_finished);

    write(
        &mut control,
        &Request::Execute {
            execution: 19,
            command: concat!(
                "printf '%s\\n%s\\n%s\\n' \"$RHO_PAGER_SOCKET\" \"$RHO_PAGER_TOKEN\" ",
                "\"$RHO_PAGER_EXECUTION_TOKEN\" >pager-auth; ",
                "command sh -c 'sleep 0.3; printf delayed | \"$PAGER\"' &"
            )
            .into(),
        },
    );
    let mut delayed = Vec::new();
    let mut delayed_started = false;
    let mut delayed_finished = false;
    let mut execution_finished = false;
    while !(delayed_started
        && delayed_finished
        && execution_finished
        && delayed.ends_with(b"delayed"))
    {
        match read(&mut control) {
            Response::Started { execution: 19 } => {}
            Response::PagerStarted { execution: 19, .. } => delayed_started = true,
            Response::PagerFinished { execution: 19, .. } => delayed_finished = true,
            Response::Output {
                execution: 19,
                data,
            } => delayed.extend(data),
            Response::Finished { execution: 19, .. } => execution_finished = true,
            frame => panic!("unexpected delayed pager frame: {frame:?}"),
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    let auth = std::fs::read_to_string(temp.path().join("pager-auth")).unwrap();
    let mut auth = auth.lines();
    let socket = auth.next().unwrap();
    let token = auth.next().unwrap();
    let execution_token = auth.next().unwrap();
    let mut stale = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stale
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    rho_shell_proto::write_pager_frame(
        &mut stale,
        &rho_shell_proto::PagerMessage::Hello {
            protocol: 1,
            token: token.into(),
            execution_token: execution_token.into(),
        },
    )
    .unwrap();
    let mut byte = [0];
    assert_eq!(std::io::Read::read(&mut stale, &mut byte).unwrap(), 0);

    write(&mut control, &Request::Shutdown);
    while !matches!(read(&mut control), Response::Exited { .. }) {}
    assert!(child.wait().unwrap().success());
    assert!(
        std::fs::read_dir(temp.path()).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            !name.starts_with("rho-pager-") || !name.ends_with(".sock")
        }),
        "rho-shell left its pager socket behind"
    );
}

fn write(control: &mut std::os::unix::net::UnixStream, request: &Request) {
    rho_shell_proto::write_frame(control, request).unwrap();
}

fn read(control: &mut std::os::unix::net::UnixStream) -> Response {
    rho_shell_proto::read_frame(control).unwrap()
}
