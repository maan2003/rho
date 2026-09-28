use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use rho_notebook::process::Client;
use tokio::sync::Notify;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notebook_process_retains_python_state_and_reports_over_typed_rpc() {
    let (owner, child_socket) = UnixStream::pair().unwrap();
    let fd = child_socket.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rho-notebook-test-child"));
    command
        .arg("/tmp")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(child_socket);
    let wake = Arc::new(Notify::new());
    let client = Client::connect(owner, Arc::clone(&wake)).unwrap();

    let cell = client.run("import threading\nvalue = [41]\nthread = threading.Thread(target=lambda: value.__setitem__(0, 42))\nthread.start()\nthread.join()".into()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.cell_facts(cell).unwrap().finished.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client.report().unwrap().unwrap().render().text,
        "Task finished"
    );
    let next = client.run("print(value[0])".into()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.cell_facts(next).unwrap().finished.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(client.report().unwrap().unwrap().render().text, "42");

    let command = client.run("job = command('sleep 1')".into()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.cell_facts(command).unwrap().returned.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(client.prepare().unwrap_err(), "host work is still running");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !client
            .facts()
            .unwrap()
            .iter()
            .any(|source| source.kind == rho_notebook::Kind::Command && source.finished.is_some())
        {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    client.prepare().unwrap();
    assert_eq!(
        client.run("print('blocked')".into()).unwrap_err(),
        "notebook is prepared for checkpoint"
    );
    client.resume().unwrap();

    client.shutdown().unwrap();
    assert!(child.wait().unwrap().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_notebook_resumes_live_python_thread() {
    let Ok(criu) = std::env::var("RHO_CRIU_TEST") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let images = dir.path().join("images");
    std::fs::create_dir(&images).unwrap();
    let (owner, child_socket) = UnixStream::pair().unwrap();
    let fd = child_socket.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rho-notebook-test-child"));
    command
        .arg("/tmp")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(child_socket);
    let ino = std::fs::read_link(format!("/proc/{}/fd/3", child.id())).unwrap();
    let ino = ino.to_string_lossy().to_string();
    assert!(ino.starts_with("socket:["), "{ino}");
    let wake = Arc::new(Notify::new());
    let client = Client::connect(owner, Arc::clone(&wake)).unwrap();
    let file = dir.path().join("partial.txt");
    let cell = client.run(format!(
        "import threading\nvalue=[41]\nf=open({:?}, 'w+')\nf.write('alpha')\nf.flush()\ngate=threading.Event()\nt=threading.Thread(target=lambda: (gate.wait(), value.__setitem__(0,42)))\nt.start()",
        file.to_str().unwrap(),
    )).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.cell_facts(cell).unwrap().returned.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert!(client.cell_facts(cell).unwrap().finished.is_some());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha");

    client.prepare().unwrap();
    let dump = Command::new(&criu)
        .args([
            "dump",
            "--unprivileged",
            "-t",
            &child.id().to_string(),
            "-D",
            images.to_str().unwrap(),
            "-o",
            "dump.log",
            "-v4",
            "--external",
            &format!("unix[{}]", &ino[8..ino.len() - 1]),
        ])
        .output()
        .unwrap();
    assert!(
        dump.status.success(),
        "dump: {}\n{}",
        String::from_utf8_lossy(&dump.stderr),
        std::fs::read_to_string(images.join("dump.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("Error ("))
            .collect::<Vec<_>>()
            .join("\\n")
    );
    child.wait().unwrap();
    drop(client);

    // The owner creates an entirely new pair. CRIU substitutes its endpoint
    // for the old socket as the notebook's original fd.
    let (owner, restored_socket) = UnixStream::pair().unwrap();
    let fd = restored_socket.as_raw_fd();
    let mut restore = Command::new(&criu);
    restore.args([
        "restore",
        "--unprivileged",
        "-d",
        "-D",
        images.to_str().unwrap(),
        "-o",
        "restore.log",
        "-v4",
        "--pidfile",
        dir.path().join("pid").to_str().unwrap(),
        "--inherit-fd",
        &format!("fd[4]:{ino}"),
    ]);
    unsafe {
        restore.pre_exec(move || {
            if libc::dup2(fd, 4) < 0 || libc::fcntl(4, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let result = restore.output().unwrap();
    assert!(
        result.status.success(),
        "restore: {}\n{}",
        String::from_utf8_lossy(&result.stderr),
        std::fs::read_to_string(images.join("restore.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("Error ("))
            .collect::<Vec<_>>()
            .join("\\n")
    );
    drop(restored_socket);
    let wake = Arc::new(Notify::new());
    let client = Client::connect(owner, Arc::clone(&wake)).unwrap();
    client.resume().unwrap();
    assert!(client.report().unwrap().is_some());
    let next = client
        .run("print(t.is_alive()); gate.set(); t.join(); f.write('beta'); f.flush(); print(value[0])".into())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.cell_facts(next).unwrap().finished.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(client.report().unwrap().unwrap().render().text, "True\n42");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "alphabeta");
    client.shutdown().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_and_owner_services_are_acknowledged_before_checkpoint() {
    use rho_fs_view::PathOverrides;
    use rho_notebook::Notebook;
    use rho_notebook::process::{self, Event, ServiceClient, ServiceKind};
    use rho_tool_shell::ShellTools;
    use tokio::sync::mpsc;

    let (owner, child) = UnixStream::pair().unwrap();
    let wake = Arc::new(Notify::new());
    let shell = ShellTools::in_directory(
        Duration::from_secs(5),
        "/tmp".into(),
        PathOverrides::default(),
    );
    let notebook = Notebook::new(shell.clone(), vec![], Arc::clone(&wake)).unwrap();
    let fresh_wake = Arc::clone(&wake);
    let factory = Box::new(move || Notebook::new(shell.clone(), vec![], Arc::clone(&fresh_wake)));
    let (send_event, receive_event) = mpsc::unbounded_channel();
    let (service, receive_service) = ServiceClient::channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (service_tx, mut service_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(process::serve_with_services(
        notebook,
        child,
        Arc::clone(&wake),
        Some(factory),
        Some(receive_event),
        Some(receive_service),
    ));
    let client = process::Client::connect_with_services(
        owner,
        Arc::clone(&wake),
        Some(event_tx),
        Some(service_tx),
    )
    .unwrap();

    send_event
        .send(Event::Send {
            cell: 17,
            text: "a message".into(),
        })
        .unwrap();
    let (id, event) = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, Event::Send { cell: 17, text } if text == "a message"));
    assert_eq!(
        client.prepare().unwrap_err(),
        "notebook events awaiting agent log"
    );
    client.ack(id).unwrap();
    assert_eq!(client.ack(id).unwrap_err(), "unknown notebook event");

    let request = tokio::spawn(async move {
        service
            .request(ServiceKind::WebCredentials, vec![1, 2, 3])
            .await
    });
    let received = tokio::time::timeout(Duration::from_secs(5), service_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.kind, ServiceKind::WebCredentials);
    assert_eq!(received.payload, vec![1, 2, 3]);
    assert_eq!(
        client.prepare().unwrap_err(),
        "notebook services awaiting owner"
    );
    client.service_reply(received.id, Ok(vec![9, 8])).unwrap();
    assert_eq!(request.await.unwrap().unwrap(), vec![9, 8]);
    assert_eq!(
        client.service_reply(received.id, Ok(vec![])).unwrap_err(),
        "unknown notebook service request"
    );

    client.prepare().unwrap();
    send_event.send(Event::EndTurn).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(80), event_rx.recv())
            .await
            .is_err()
    );
    assert_eq!(
        client.prepare().unwrap_err(),
        "notebook events awaiting agent log"
    );
    client.resume().unwrap();
    let (second_id, event) = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(id, second_id);
    assert!(matches!(event, Event::EndTurn));
    client.ack(second_id).unwrap();
    assert_eq!(client.checkin().unwrap(), Duration::from_secs(120));
    let cell = client.stream().unwrap();
    assert_eq!(client.latest_cell().unwrap().unwrap().0, cell);
    client.feed(cell, "a = 1\nb = ".into(), false).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.progress(cell).unwrap().settled == 0 {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(client.interrupt(cell).unwrap(), Some("a = 1\n".len()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.cell_facts(cell).unwrap().finished.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    client.report().unwrap();
    client.fresh().unwrap();
    assert!(client.latest_cell().unwrap().is_none());
    let next = client
        .run("print('fresh', 'a' in globals())".into())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.cell_facts(next).unwrap().finished.is_none() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client.report().unwrap().unwrap().render().text,
        "fresh False"
    );
    client.shutdown().unwrap();
    server.await.unwrap().unwrap();
}
