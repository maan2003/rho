//! Shell commands as sources.
//!
//! `exec_command` starts a process and stays attached to it for the process's
//! whole life: the call is answered whenever the next request goes out — with
//! the finished result if the command is done, otherwise with what it has said
//! so far and a session ID — and everything the process says after that lands
//! on the same call as updates. `write_stdin` types into a session; it never
//! polls, because there is nothing to poll.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rho_agent2::{SourceWaker, Tool, ToolHaste, ToolSession};
use rho_core::{ToolCall, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType, UnixMs};
use rho_tool_shell::{
    APPLY_PATCH_TOOL_NAME, BoundedOutput, EXEC_COMMAND_TOOL_NAME, ProcessEvent, ShellTools,
    WRITE_STDIN_TOOL_NAME, decode_output_lossy,
};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Notify;

use crate::{Finished, OneShot, output, stands_on_its_own};

/// One of the three shell tools. They share the session table, so a process
/// started by one `exec_command` call can be typed into by `write_stdin`.
pub struct ShellTool {
    shared: Arc<Shared>,
    name: &'static str,
}

struct Shared {
    tools: ShellTools,
    next_id: AtomicI32,
    sessions: Mutex<HashMap<i32, Arc<Link>>>,
}

/// What `write_stdin` can reach of a running process.
struct Link {
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
}

impl ShellTool {
    pub fn all(tools: ShellTools) -> Vec<Arc<dyn Tool>> {
        let shared = Arc::new(Shared {
            tools,
            next_id: AtomicI32::new(1),
            sessions: Mutex::new(HashMap::new()),
        });
        [
            EXEC_COMMAND_TOOL_NAME,
            WRITE_STDIN_TOOL_NAME,
            APPLY_PATCH_TOOL_NAME,
        ]
        .into_iter()
        .map(|name| {
            Arc::new(ShellTool {
                shared: Arc::clone(&shared),
                name,
            }) as Arc<dyn Tool>
        })
        .collect()
    }
}

#[derive(Deserialize)]
struct ExecArgs {
    #[serde(alias = "command")]
    cmd: String,
    #[serde(alias = "cwd")]
    workdir: Option<String>,
    max_output_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct WriteStdinArgs {
    session_id: i32,
    #[serde(default)]
    chars: String,
}

impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        match self.name {
            EXEC_COMMAND_TOOL_NAME => ToolSpec {
                name: ToolName::try_from(EXEC_COMMAND_TOOL_NAME).expect("valid name"),
                tool_type: ToolType::Function,
                description: "Runs a shell command. The result arrives when the command finishes, \
                              however long that takes. A command still running when something \
                              else needs your attention is answered with a session ID and what it \
                              has printed so far; everything it prints after that arrives on this \
                              same call by itself, so never poll. Use write_stdin to type into a \
                              session."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["cmd"],
                    "properties": {
                        "cmd": {"type": "string", "description": "Command line, run with bash -c."},
                        "workdir": {"type": "string", "description": "Working directory. Defaults to the agent's primary workdir."},
                        "max_output_tokens": {"type": "integer", "description": "Budget for each chunk of output. Defaults to 10000."}
                    }
                }),
                format: None,
            },
            WRITE_STDIN_TOOL_NAME => ToolSpec {
                name: ToolName::try_from(WRITE_STDIN_TOOL_NAME).expect("valid name"),
                tool_type: ToolType::Function,
                description: "Types into a running exec_command session. Whatever the process \
                              prints in response arrives on the exec_command call that started \
                              it, not here."
                    .to_owned(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["session_id", "chars"],
                    "properties": {
                        "session_id": {"type": "integer"},
                        "chars": {"type": "string", "description": "Bytes to write, newline included if the program expects one."}
                    }
                }),
                format: None,
            },
            _ => self.shared.tools.apply_patch_spec(),
        }
    }

    fn run(&self, call: ToolCall, waker: SourceWaker) -> Box<dyn ToolSession> {
        match self.name {
            EXEC_COMMAND_TOOL_NAME => match serde_json::from_str::<ExecArgs>(&call.arguments) {
                Ok(args) => Box::new(ExecSession::start(Arc::clone(&self.shared), args, waker)),
                Err(error) => Box::new(Finished::error(format!(
                    "bad exec_command arguments: {error}"
                ))),
            },
            WRITE_STDIN_TOOL_NAME => {
                let args = match serde_json::from_str::<WriteStdinArgs>(&call.arguments) {
                    Ok(args) => args,
                    Err(error) => {
                        return Box::new(Finished::error(format!(
                            "bad write_stdin arguments: {error}"
                        )));
                    }
                };
                let link = self
                    .shared
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&args.session_id)
                    .cloned();
                let Some(link) = link else {
                    return Box::new(Finished::error(format!(
                        "unknown session ID {}; it has ended or never existed",
                        args.session_id
                    )));
                };
                if args.chars.is_empty() {
                    return Box::new(Finished::new(output(
                        format!(
                            "Nothing written. Output from session {} arrives on its exec_command \
                             call by itself; call wait if you want to be woken later.",
                            args.session_id
                        ),
                        ToolOutputStatus::Success,
                    )));
                }
                Box::new(OneShot::spawn(waker, async move {
                    let mut stdin = link.stdin.lock().await;
                    let Some(stdin) = stdin.as_mut() else {
                        return output("session stdin is closed", ToolOutputStatus::Error);
                    };
                    match async {
                        stdin.write_all(args.chars.as_bytes()).await?;
                        stdin.flush().await
                    }
                    .await
                    {
                        Ok(()) => output(
                            format!(
                                "Wrote {} bytes to session {}.",
                                args.chars.len(),
                                args.session_id
                            ),
                            ToolOutputStatus::Success,
                        ),
                        Err(error) => {
                            output(format!("write failed: {error}"), ToolOutputStatus::Error)
                        }
                    }
                }))
            }
            _ => {
                let tools = self.shared.tools.clone();
                Box::new(OneShot::spawn(waker, async move { tools.call(call).await }))
            }
        }
    }
}

// -- exec_command -----------------------------------------------------------

/// What the reader task has collected and the session has not yet handed
/// over.
struct ExecState {
    unsent: BoundedOutput,
    /// When the unsent output started standing on its own, if it does.
    soon_since: Option<UnixMs>,
    exit: Option<Exit>,
    /// Both pipes drained after exit, or the process could not be followed:
    /// nothing more will come.
    closed_at: Option<UnixMs>,
    /// Why the process could not be started or waited on.
    failure: Option<String>,
}

#[derive(Clone, Copy)]
struct Exit {
    code: Option<i32>,
    terminated: bool,
}

struct ExecSession {
    shared: Arc<Shared>,
    state: Arc<Mutex<ExecState>>,
    link: Arc<Link>,
    cancel: Arc<Notify>,
    started: Instant,
    max_output_tokens: Option<usize>,
    /// The ID handed out when the call was answered while still running.
    session_id: Option<i32>,
    answered: bool,
    exit_reported: bool,
}

impl ExecSession {
    fn start(shared: Arc<Shared>, args: ExecArgs, waker: SourceWaker) -> Self {
        let state = Arc::new(Mutex::new(ExecState {
            unsent: BoundedOutput::for_tokens(args.max_output_tokens),
            soon_since: None,
            exit: None,
            closed_at: None,
            failure: None,
        }));
        let link = Arc::new(Link {
            stdin: tokio::sync::Mutex::new(None),
        });
        let cancel = Arc::new(Notify::new());
        tokio::spawn(follow(
            Arc::clone(&shared),
            args.cmd,
            args.workdir,
            Arc::clone(&state),
            Arc::clone(&link),
            Arc::clone(&cancel),
            waker,
        ));
        Self {
            shared,
            state,
            link,
            cancel,
            started: Instant::now(),
            max_output_tokens: args.max_output_tokens,
            session_id: None,
            answered: false,
            exit_reported: false,
        }
    }

    /// Everything unsent, rendered the way `exec_command` always has so the
    /// model reads it the same whichever loop is driving.
    fn render(&mut self, first: bool) -> ToolOutput {
        let mut state = self.state.lock().unwrap();
        let unsent = std::mem::replace(
            &mut state.unsent,
            BoundedOutput::for_tokens(self.max_output_tokens),
        );
        state.soon_since = None;
        let closed = state.closed_at.is_some();
        let exit = state.exit;
        let failure = state.failure.clone();
        drop(state);

        let mut lines = Vec::new();
        if first {
            lines.push(format!(
                "Wall time: {:.4} seconds",
                self.started.elapsed().as_secs_f64()
            ));
        }
        let mut status = ToolOutputStatus::Success;
        if closed {
            self.exit_reported = true;
            self.session_id
                .take()
                .map(|id| self.shared.sessions.lock().unwrap().remove(&id));
            match (exit, failure) {
                (
                    Some(Exit {
                        terminated: true, ..
                    }),
                    _,
                ) => lines.push("Process terminated".to_owned()),
                (
                    Some(Exit {
                        code: Some(code), ..
                    }),
                    _,
                ) => lines.push(format!("Process exited with code {code}")),
                (Some(Exit { code: None, .. }), _) => {
                    lines.push("Process killed by signal".to_owned())
                }
                (None, Some(failure)) => {
                    status = ToolOutputStatus::Error;
                    lines.push(format!("Process could not run: {failure}"));
                }
                (None, None) => lines.push("Process ended".to_owned()),
            }
        } else {
            let id = *self.session_id.get_or_insert_with(|| {
                let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
                self.shared
                    .sessions
                    .lock()
                    .unwrap()
                    .insert(id, Arc::clone(&self.link));
                id
            });
            lines.push(format!("Process running with session ID {id}"));
        }
        if !unsent.is_empty() {
            lines.push("Output:".to_owned());
            lines.push(decode_output_lossy(unsent.into_bytes()));
        } else if first {
            lines.push("Output:".to_owned());
            lines.push(String::new());
        }
        output(lines.join("\n"), status)
    }
}

/// Reads the process until it closes or the session is cancelled, and wakes
/// the core at every event: what any of it is worth is the core's call.
async fn follow(
    shared: Arc<Shared>,
    cmd: String,
    workdir: Option<String>,
    state: Arc<Mutex<ExecState>>,
    link: Arc<Link>,
    cancel: Arc<Notify>,
    waker: SourceWaker,
) {
    let mut process = match shared.tools.spawn(&cmd, workdir.as_deref()).await {
        Ok(process) => process,
        Err(error) => {
            let mut state = state.lock().unwrap();
            state.failure = Some(error.to_string());
            state.closed_at = Some(UnixMs::now());
            drop(state);
            waker.wake();
            return;
        }
    };
    *link.stdin.lock().await = process.take_stdin();
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.notified() => {
                // Dropping the process kills it; say so rather than wait for
                // the pipes, which a grandchild could hold open.
                drop(process);
                let mut state = state.lock().unwrap();
                state.exit = Some(Exit { code: None, terminated: true });
                state.closed_at = Some(UnixMs::now());
                drop(state);
                waker.wake();
                return;
            }
            event = process.next() => event,
        };
        let mut st = state.lock().unwrap();
        match event {
            ProcessEvent::Output(chunk) => {
                if st.soon_since.is_none() && stands_on_its_own(&String::from_utf8_lossy(&chunk)) {
                    st.soon_since = Some(UnixMs::now());
                }
                st.unsent.push(&chunk);
            }
            ProcessEvent::Exited(status) => {
                st.exit = Some(Exit {
                    code: status.code(),
                    terminated: false,
                });
            }
            ProcessEvent::Failed(error) => {
                st.failure = Some(error);
                st.closed_at = Some(UnixMs::now());
                drop(st);
                waker.wake();
                return;
            }
            ProcessEvent::Closed => {
                st.closed_at = Some(UnixMs::now());
                drop(st);
                waker.wake();
                return;
            }
        }
        drop(st);
        waker.wake();
    }
}

impl ToolSession for ExecSession {
    fn haste(&self) -> ToolHaste {
        let state = self.state.lock().unwrap();
        match (state.closed_at, state.soon_since) {
            (Some(at), _) => ToolHaste::Ended { at },
            (None, Some(since)) => ToolHaste::Soon { since },
            // Output from a command still running is not news until the
            // command ends or the model looks in.
            (None, None) => ToolHaste::None,
        }
    }

    fn done(&self) -> bool {
        self.answered && self.exit_reported && self.state.lock().unwrap().unsent.is_empty()
    }

    fn first_output(&mut self) -> ToolOutput {
        self.answered = true;
        self.render(true)
    }

    fn more_output(&mut self) -> Option<ToolOutput> {
        let state = self.state.lock().unwrap();
        let something_new =
            !state.unsent.is_empty() || (state.closed_at.is_some() && !self.exit_reported);
        drop(state);
        something_new.then(|| self.render(false))
    }

    fn cancel(&mut self) {
        self.cancel.notify_one();
    }
}

impl Drop for ExecSession {
    fn drop(&mut self) {
        self.cancel.notify_one();
        if let Some(id) = self.session_id {
            self.shared.sessions.lock().unwrap().remove(&id);
        }
    }
}
