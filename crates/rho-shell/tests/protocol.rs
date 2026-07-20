use std::os::unix::process::CommandExt as _;
use std::process::Stdio;
use std::time::Duration;

use rho_shell_proto::{PROTOCOL_VERSION, Request, Response};

#[test]
fn output_events_keep_execution_boundaries_and_merge_standard_streams() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".bashrc"), "PS1='test> '\n").unwrap();

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

    let Response::Ready { protocol, .. } = read(&mut control) else {
        panic!("rho-shell did not send Ready");
    };
    assert_eq!(protocol, PROTOCOL_VERSION);

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

    write(&mut control, &Request::Shutdown);
    while !matches!(read(&mut control), Response::Exited { .. }) {}
    assert!(child.wait().unwrap().success());
}

fn write(control: &mut std::os::unix::net::UnixStream, request: &Request) {
    rho_shell_proto::write_frame(control, request).unwrap();
}

fn read(control: &mut std::os::unix::net::UnixStream) -> Response {
    rho_shell_proto::read_frame(control).unwrap()
}
