//! The shell registry end to end: a real rho-shell sidecar (this binary,
//! re-executed) inside a view-mode namespace over a temporary workset.
//! Harness-free because the identity user namespace must precede every
//! thread, including the test harness's.

use std::sync::Arc;

use rho_daemon::shell::{ShellClient, ShellControl, ShellRegistry, ShellSpawn};
use rho_ui_proto::AgentId;
use rho_ui_proto::shell::{ShellColor, ShellServerFrame};

/// This binary is the sidecar when started with this argument.
const CHILD_FLAG: &str = "--rho-shell-child";

fn main() {
    if std::env::args().nth(1).as_deref() == Some(CHILD_FLAG) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(rho_shell::run())
            .unwrap();
        return;
    }
    let unshare = std::process::Command::new("unshare")
        .args(["-U", "true"])
        .status();
    if !unshare.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping shell_e2e: kernel forbids unshare(CLONE_NEWUSER)");
        return;
    }
    // SAFETY: top of main, before the runtime: no threads exist yet.
    unsafe { rho_fs_view::init_daemon_namespace() }.unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(shell_end_to_end_over_registry());
    println!("shell e2e passed");
}

async fn shell_end_to_end_over_registry() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".bashrc"),
        "PS1='rho-test> '\n\
         PROMPT_COMMAND='export RHO_TEST_CONFIG_HOOK=fired'\n\
         trap 'printf fired >/src/brush-exit-hook' EXIT\n",
    )
    .unwrap();
    let environment = rho_fs_view::UserEnvironment::new(vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap()),
        ("HOME".into(), home.clone().into_os_string()),
        ("USER".into(), "rho-test".into()),
        ("LOGNAME".into(), "rho-test".into()),
        ("LANG".into(), "C.UTF-8".into()),
    ]);
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let worksets = rho_fs_view::Worksets::open(
        temp.path().join("state"),
        environment,
        Default::default(),
        rho_fs_view::StoreService::None,
    )
    .await
    .unwrap();
    // View mode: `home` is the skeleton of the agent's /home/agent, and this
    // binary's directory is visible read-only, so it can be the sidecar.
    let view = worksets
        .adopt(&work)
        .unwrap()
        .enter(
            rho_fs_view::Mode::View {
                home_skeleton: Some(home.clone()),
            },
            camino::Utf8Path::new(rho_fs_view::MOUNT_ROOT),
        )
        .unwrap();
    let registry = Arc::new(ShellRegistry::default());
    let agent_id =
        AgentId::from_counter(1, &rho_agent::db::AgentIdDomain(42)).expect("counter encodes");

    let spawn = || ShellSpawn {
        view: Arc::clone(&view),
        program: std::env::current_exe().unwrap().into_os_string(),
        args: vec![CHILD_FLAG.into()],
        pager_program: "cat".into(),
    };
    registry.start(agent_id, spawn()).await.unwrap();
    let mut first = registry.attach(agent_id).await.unwrap();
    let entries = registry.list().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].agent_id, agent_id);
    assert_eq!(entries[0].clients, 1);
    let mut first_state = rho_ui_proto::shell::ShellState::default();

    let initial_token = "shell-e2e-23";
    let initial_command = shell_token(initial_token);
    first.submit.send(initial_command.clone()).await.unwrap();
    wait_for_text(&mut first, &mut first_state, initial_token).await;
    assert_eq!(
        render_state(&first_state).matches(&initial_command).count(),
        1,
        "the sideband protocol is the only source of accepted input"
    );

    first
        .submit
        .send("printf 'pager-env-%s-%s' \"$PAGER\" \"$GIT_PAGER\"".to_owned())
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, "pager-env-cat-cat").await;

    // Brush owns one persistent evaluator, including variables, functions,
    // working directory, startup configuration, and prompt hooks.
    let state_prefix = shell_token("kernel-state-");
    first
        .submit
        .send(format!(
            "rho_kernel_value=persistent; rho_kernel_fn() {{ {state_prefix} \"$rho_kernel_value\"; }}"
        ))
        .await
        .unwrap();
    first.submit.send("rho_kernel_fn".to_owned()).await.unwrap();
    wait_for_text(&mut first, &mut first_state, "kernel-state-persistent").await;

    first
        .submit
        .send("mkdir -p nested; cd nested".to_owned())
        .await
        .unwrap();
    let cwd_token = "cwd-persisted";
    first
        .submit
        .send(format!(
            "test \"$(basename \"$PWD\")\" = nested && {}",
            shell_token(cwd_token)
        ))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, cwd_token).await;
    first.submit.send("cd ..".to_owned()).await.unwrap();

    let prompt_token = "prompt-hook-fired";
    first
        .submit
        .send(format!(
            "{} \"$RHO_TEST_CONFIG_HOOK\"",
            shell_token("prompt-hook-")
        ))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, prompt_token).await;

    let color_token = "colored-shell-output";
    let color_execution = first
        .submit
        .send(format!("printf '\\033[31m{color_token}\\033[0m\\n'"))
        .await
        .unwrap();
    wait_for_finished(&mut first, &mut first_state, color_execution).await;
    let color_block = first_state
        .executions
        .iter()
        .find(|block| block.execution == color_execution)
        .unwrap();
    let color_start = color_block.output.find(color_token).unwrap() as u64;
    assert!(color_block.styles.iter().any(|span| {
        span.start <= color_start
            && span.end >= color_start + color_token.len() as u64
            && span.style.foreground == Some(ShellColor::Indexed(1))
    }));
    let color_env_token = "color-environment-ok";
    first
        .submit
        .send(format!(
            "test \"$TERM\" = xterm-256color && test -z \"${{NO_COLOR+x}}\" && {}",
            shell_token(color_env_token)
        ))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, color_env_token).await;

    // Prompt construction is bounded independently of command execution.
    first
        .submit
        .send("printf -v PS1 '%20000s' ''".to_owned())
        .await
        .unwrap();
    let bounded_prompt_token = "bounded-prompt-survived";
    first
        .submit
        .send(format!(
            "PS1='rho-test> '; {}",
            shell_token(bounded_prompt_token)
        ))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, bounded_prompt_token).await;

    // The protocol descriptor is close-on-exec and is not present in the
    // virtual descriptor table used to execute commands.
    let control_fd_token = "control-fd-closed";
    first
        .submit
        .send(format!(
            "command sh -c 'test ! -e /proc/self/fd/3' && {}",
            shell_token(control_fd_token)
        ))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, control_fd_token).await;

    // A background writer retains the execution that created its PTY,
    // even when its bytes arrive after the foreground evaluator returned.
    let late_token = "tagged-late-output";
    first
        .submit
        .send(format!("{{ sleep 0.1; {}; }} &", shell_token(late_token)))
        .await
        .unwrap();
    let foreground_token = "tagged-foreground-output";
    first
        .submit
        .send(shell_token(foreground_token))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, foreground_token).await;
    wait_for_text(&mut first, &mut first_state, late_token).await;

    // Interrupt targets descendants attached to the active execution PTY
    // without giving the persistent evaluator a controlling terminal.
    let started_token = "foreground-started";
    first
        .submit
        .send(format!(
            "{}; sleep 60 && touch interrupt-failed",
            shell_token(started_token)
        ))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, started_token).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    first.control.send(ShellControl::Interrupt).await.unwrap();
    let interrupt_token = "interrupt-ok";
    first
        .submit
        .send(shell_token(interrupt_token))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, interrupt_token).await;
    assert!(!work.join("interrupt-failed").exists());

    let line_token = "line-limit-ok";
    let long_line = format!("{} #{}", shell_token(line_token), "x".repeat(8192));
    assert!(rho_ui_proto::shell::command_fits(&long_line));
    first.submit.send(long_line).await.unwrap();
    wait_for_text(&mut first, &mut first_state, line_token).await;

    let too_long = format!(
        "touch oversized-command-ran #{}",
        "x".repeat(rho_ui_proto::shell::MAX_COMMAND_BYTES)
    );
    assert!(!rho_ui_proto::shell::command_fits(&too_long));
    assert!(first.submit.send(too_long).await.is_err());
    let after_oversized_token = "after-oversized-ok";
    first
        .submit
        .send(shell_token(after_oversized_token))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, after_oversized_token).await;
    assert!(!work.join("oversized-command-ran").exists());
    wait_for_idle(&first).await;

    // Controls sent while idle are scoped to no execution.
    first.control.send(ShellControl::Interrupt).await.unwrap();
    first.control.send(ShellControl::Eof).await.unwrap();
    let idle_control_token = "idle-controls-discarded";
    first
        .submit
        .send(format!("sleep 0.1; {}", shell_token(idle_control_token)))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, idle_control_token).await;

    // EOF writes VEOF only to this execution's PTY, not the shell session.
    let cat_execution = first.submit.send("cat".to_owned()).await.unwrap();
    wait_for_running(&mut first, &mut first_state, cat_execution).await;
    first.control.send(ShellControl::Eof).await.unwrap();
    let after_eof_token = "after-eof";
    let after_eof_execution = first
        .submit
        .send(shell_token(after_eof_token))
        .await
        .unwrap();
    wait_for_text(&mut first, &mut first_state, after_eof_token).await;
    wait_for_finished(&mut first, &mut first_state, after_eof_execution).await;

    // A later attachment receives the canonical structured snapshot.
    let mut second = registry.attach(agent_id).await.unwrap();
    assert_eq!(registry.list().await[0].clients, 2);
    let mut second_state = rho_ui_proto::shell::ShellState::default();
    wait_for_text(&mut second, &mut second_state, initial_token).await;
    assert_eq!(second_state, first_state);

    let final_token = "final-output-before-exit";
    first
        .submit
        .send(format!(
            "sh -c 'echo $$ > bg.pid; exec sleep 60' & sleep 0.1; {}; exit 7",
            shell_token(final_token)
        ))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), first.exit.changed())
        .await
        .expect("shell must exit")
        .expect("shell exit watch remains open");
    assert_eq!(
        first.exit.borrow().as_ref().and_then(|exit| exit.status),
        Some(7)
    );
    let exit = first.exit.borrow();
    let state = &exit.as_ref().unwrap().state;
    assert!(
        state
            .executions
            .windows(2)
            .all(|pair| pair[0].execution < pair[1].execution)
    );
    let final_execution = state.executions.last().unwrap();
    assert!(final_execution.output.contains(final_token));
    assert_eq!(
        final_execution.state,
        rho_ui_proto::shell::ShellExecutionState::Finished { status: 7 }
    );
    drop(exit);
    assert_eq!(
        std::fs::read_to_string(work.join("brush-exit-hook"))
            .unwrap()
            .trim(),
        "fired"
    );

    let background_pid: i32 = std::fs::read_to_string(work.join("bg.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let gone = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if unsafe { libc::kill(background_pid, 0) } < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "background shell session member survived exit"
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !registry.list().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("exited shell remained registered");

    // Starting, detaching, discovering, and explicitly closing are
    // separate daemon lifecycle operations.
    registry.start(agent_id, spawn()).await.unwrap();
    let detached = registry.attach(agent_id).await.unwrap();
    assert_eq!(registry.list().await[0].clients, 1);
    drop(detached);
    assert_eq!(registry.list().await[0].clients, 0);
    let closing = registry.attach(agent_id).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), registry.close(agent_id))
        .await
        .expect("explicit shell close timed out")
        .unwrap();
    assert!(closing.exit.borrow().is_some());
    assert!(registry.list().await.is_empty());
}

fn shell_token(token: &str) -> String {
    let arguments = token
        .chars()
        .map(|character| format!("'{character}'"))
        .collect::<Vec<_>>()
        .join(" ");
    let command = format!("printf '%s' {arguments}");
    assert!(!command.contains(token));
    command
}

async fn wait_for_idle(client: &ShellClient) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while client.control.active_execution() != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shell did not become idle");
}

async fn wait_for_text(
    client: &mut ShellClient,
    state: &mut rho_ui_proto::shell::ShellState,
    needle: &str,
) {
    if render_state(state).contains(needle) {
        return;
    }
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            apply_test_frame(
                state,
                client.frames.recv().await.expect("shell stream ended"),
            );
            if render_state(state).contains(needle) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {needle:?} in {:?}", render_state(state)));
}

async fn wait_for_running(
    client: &mut ShellClient,
    state: &mut rho_ui_proto::shell::ShellState,
    execution: u64,
) {
    let running = |state: &rho_ui_proto::shell::ShellState| {
        state.executions.iter().any(|block| {
            block.execution == execution
                && matches!(
                    block.state,
                    rho_ui_proto::shell::ShellExecutionState::Running
                )
        })
    };
    if running(state) {
        return;
    }
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !running(state) {
            apply_test_frame(
                state,
                client.frames.recv().await.expect("shell stream ended"),
            );
        }
    })
    .await
    .expect("execution did not start");
}

async fn wait_for_finished(
    client: &mut ShellClient,
    state: &mut rho_ui_proto::shell::ShellState,
    execution: u64,
) {
    let finished = |state: &rho_ui_proto::shell::ShellState| {
        state.executions.iter().any(|block| {
            block.execution == execution
                && matches!(
                    block.state,
                    rho_ui_proto::shell::ShellExecutionState::Finished { .. }
                )
        })
    };
    if finished(state) {
        return;
    }
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !finished(state) {
            apply_test_frame(
                state,
                client.frames.recv().await.expect("shell stream ended"),
            );
        }
    })
    .await
    .expect("execution did not finish");
}

fn apply_test_frame(state: &mut rho_ui_proto::shell::ShellState, frame: ShellServerFrame) {
    use rho_ui_proto::shell::ShellExecutionState;
    match frame {
        ShellServerFrame::Snapshot { state: snapshot } => *state = snapshot,
        ShellServerFrame::ExecutionQueued { execution } => {
            state.executions.push(execution);
        }
        ShellServerFrame::ExecutionStarted {
            execution,
            prompt,
            cwd,
        } => {
            let block = state
                .executions
                .iter_mut()
                .find(|block| block.execution == execution)
                .expect("started execution was queued");
            block.state = ShellExecutionState::Running;
            block.prompt = prompt;
            block.cwd = cwd;
        }
        ShellServerFrame::ExecutionOutput {
            execution,
            start,
            end,
            text,
            styles,
        } => {
            let block = state
                .executions
                .iter_mut()
                .find(|block| block.execution == execution)
                .expect("output execution was queued");
            block
                .output
                .replace_range(start as usize..end as usize, &text);
            block.styles.retain(|span| span.end <= start);
            block.styles.extend(styles);
        }
        ShellServerFrame::PagerPaused { execution, pager } => {
            assert_eq!(pager.execution, execution);
            state
                .pagers
                .retain(|item| item.execution != execution || item.pager != pager.pager);
            state.pagers.push(pager);
        }
        ShellServerFrame::PagerResumed { execution, pager }
        | ShellServerFrame::PagerFinished { execution, pager } => {
            state
                .pagers
                .retain(|item| item.execution != execution || item.pager != pager);
        }
        ShellServerFrame::ExecutionFinished { execution, status } => {
            let block = state
                .executions
                .iter_mut()
                .find(|block| block.execution == execution)
                .expect("finished execution was queued");
            block.state = ShellExecutionState::Finished { status };
        }
        ShellServerFrame::ExecutionFailed { execution } => {
            if let Some(block) = execution.and_then(|execution| {
                state
                    .executions
                    .iter_mut()
                    .find(|block| block.execution == execution)
            }) {
                block.state = ShellExecutionState::Failed;
            }
        }
        ShellServerFrame::TerminalOutput {
            start,
            end,
            text,
            styles,
        } => {
            state
                .terminal_output
                .replace_range(start as usize..end as usize, &text);
            state.terminal_styles.retain(|span| span.end <= start);
            state.terminal_styles.extend(styles);
        }
        ShellServerFrame::Prompt { prompt, cwd } => {
            state.prompt = prompt;
            state.cwd = cwd;
        }
        ShellServerFrame::Accepted { .. } => {}
        ShellServerFrame::Exited { .. } => panic!("shell exited early"),
    }
}

fn render_state(state: &rho_ui_proto::shell::ShellState) -> String {
    let mut text = state.terminal_output.clone();
    for execution in &state.executions {
        if matches!(
            execution.state,
            rho_ui_proto::shell::ShellExecutionState::Queued
        ) {
            continue;
        }
        text.push_str(&execution.prompt);
        text.push_str(&execution.command);
        text.push('\n');
        text.push_str(&execution.output);
    }
    text
}
