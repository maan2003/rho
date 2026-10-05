use rho_fs_view::PathOverrides;
use tokio::io::AsyncWriteExt;

use super::*;

fn test_tools() -> ShellTools {
    tools_in(std::env::temp_dir())
}

fn tools_in(directory: impl Into<std::path::PathBuf>) -> ShellTools {
    ShellTools::in_directory(
        camino::Utf8PathBuf::try_from(directory.into()).unwrap(),
        PathOverrides::default(),
    )
}

/// Runs `cmd` to the end; returns its exit code and combined output.
async fn run(tools: &ShellTools, cmd: &str, workdir: Option<&str>) -> (Option<i32>, String) {
    collect(tools.spawn(cmd, workdir, false).await.unwrap()).await
}

async fn collect(mut process: SpawnedProcess) -> (Option<i32>, String) {
    let mut code = None;
    let mut output = Vec::new();
    loop {
        match process.next().await {
            ProcessEvent::Output(bytes) => output.extend(bytes),
            ProcessEvent::Exited(status) => code = status.code(),
            ProcessEvent::Closed => break,
            ProcessEvent::Failed(error) => panic!("{error}"),
        }
    }
    (code, String::from_utf8(output).unwrap())
}

#[tokio::test]
async fn a_pipeline_fails_when_any_stage_does() {
    let result = run(&test_tools(), "sh -c 'echo partial; exit 3' | cat", None).await;
    assert_eq!(result, (Some(3), "partial\n".to_owned()));
}

#[tokio::test]
async fn stdin_reaches_a_command_started_with_it() {
    let mut process = test_tools()
        .spawn("read line; printf 'got:%s' \"$line\"", None, true)
        .await
        .unwrap();
    let mut stdin = process.take_stdin().unwrap();
    stdin.write_all(b"hello\n").await.unwrap();
    assert_eq!(collect(process).await, (Some(0), "got:hello".to_owned()));
}

#[tokio::test]
async fn dropping_a_process_kills_it() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("pid");
    let mut process = test_tools()
        .spawn(
            &format!(
                "printf '%s' $$ > {}; echo ready; sleep 5",
                pid_file.display()
            ),
            None,
            false,
        )
        .await
        .unwrap();
    assert!(matches!(process.next().await, ProcessEvent::Output(_)));
    let pid = std::fs::read_to_string(pid_file).unwrap();
    drop(process);

    let proc = std::path::Path::new("/proc").join(pid);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while proc.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!proc.exists(), "dropped command was not killed");
}

#[tokio::test]
async fn shell_inherits_process_environment_and_explicit_override_wins() {
    let home = std::env::var("HOME").unwrap();
    let script = "printf '%s %s' \"$HOME\" \"$RHO_TOOL_SHELL_TEST\"";
    assert_eq!(
        run(
            &test_tools().with_env("RHO_TOOL_SHELL_TEST", "set"),
            script,
            None
        )
        .await,
        (Some(0), format!("{home} set"))
    );
    assert_eq!(
        run(
            &test_tools().with_env("HOME", "/explicit-home"),
            script,
            None
        )
        .await,
        (Some(0), "/explicit-home ".to_owned())
    );
}

#[tokio::test]
async fn workdir_is_absolute_or_relative_to_the_tools_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let left = root.join("left");
    let right = root.join("right");
    std::fs::create_dir_all(left.join("sub")).unwrap();
    std::fs::create_dir(&right).unwrap();
    let process_cwd = std::env::current_dir().unwrap();
    let tools = tools_in(left.clone());

    let pwd = |workdir| run(&tools, "pwd", workdir);
    let right = right.to_str().unwrap();
    assert_eq!(pwd(Some(right)).await, (Some(0), format!("{right}\n")));
    assert_eq!(
        pwd(Some("sub")).await,
        (Some(0), format!("{}/sub\n", left.display()))
    );
    assert_eq!(pwd(None).await, (Some(0), format!("{}\n", left.display())));
    assert_eq!(std::env::current_dir().unwrap(), process_cwd);
}

#[tokio::test]
async fn interleaves_stdout_and_stderr_in_read_order() {
    let result = run(
        &test_tools(),
        "printf out; sleep 0.05; printf err >&2; sleep 0.05; printf out2; exit 3",
        None,
    )
    .await;
    assert_eq!(result, (Some(3), "outerrout2".to_owned()));
}

#[test]
fn bounded_output_keeps_both_ends_and_counts_what_it_dropped() {
    let mut output = BoundedOutput::for_tokens(Some(100));
    output.push(b"start");
    for _ in 0..1000 {
        output.push(b"middle");
    }
    output.push(b"end");
    assert!(output.is_truncated());
    let bytes = String::from_utf8(output.into_bytes()).unwrap();
    // 6008 bytes against a 400-byte budget: 5608 dropped, 1402 tokens.
    let (head, tail) = bytes.split_once("\n…1402 tokens truncated…\n").unwrap();
    assert!(
        head.starts_with("startmiddle") && head.len() == 200,
        "{head}"
    );
    assert!(tail.ends_with("middleend") && tail.len() == 200, "{tail}");
}

#[test]
fn interpreter_cwd_is_private_to_its_thread() {
    let temp = tempfile::tempdir().unwrap();
    let process_cwd = std::env::current_dir().unwrap();
    let target = temp.path().to_owned();
    let tools = tools_in(target.clone());
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            unsafe { tools.enter_interpreter_thread().await }.unwrap();
            assert_eq!(std::env::current_dir().unwrap(), target);
        });
    })
    .join()
    .unwrap();
    assert_eq!(std::env::current_dir().unwrap(), process_cwd);
}

#[tokio::test]
async fn shell_path_overrides_prepend_and_append_entries() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let before = temp.path().join("before");
    let after = temp.path().join("after");
    std::fs::create_dir_all(&before).unwrap();
    std::fs::create_dir_all(&after).unwrap();

    let selected = before.join("rho-path-selected");
    std::fs::write(&selected, "#!/bin/sh\nprintf before\n").unwrap();
    std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o755)).unwrap();

    let shadowed = after.join("rho-path-selected");
    std::fs::write(&shadowed, "#!/bin/sh\nprintf after\n").unwrap();
    std::fs::set_permissions(&shadowed, std::fs::Permissions::from_mode(0o755)).unwrap();

    let after_only = after.join("rho-path-after-only");
    std::fs::write(&after_only, "#!/bin/sh\nprintf after-only\n").unwrap();
    std::fs::set_permissions(&after_only, std::fs::Permissions::from_mode(0o755)).unwrap();

    let tools = ShellTools::in_directory(
        camino::Utf8PathBuf::try_from(temp.path().to_path_buf()).unwrap(),
        PathOverrides {
            before: vec![before],
            after: vec![after],
        },
    );
    let result = run(
        &tools,
        "rho-path-selected; printf ' '; rho-path-after-only",
        None,
    )
    .await;
    assert_eq!(result, (Some(0), "before after-only".to_owned()));
}
