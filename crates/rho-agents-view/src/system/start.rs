//! Starting an agent from a message.
//!
//! Any message is what an agent starts from: the one under the point, or
//! the one the composer is about to send. The user picks where (a new
//! workset on a repository, a directory on the host as it is, or beside an
//! agent already there) and the runtime; the GUI sends the code, which
//! names the message as `origin`, so policy joins the agent to its thread
//! and hands it the message first.

use std::rc::Rc;

use camino::Utf8PathBuf;
use futures::FutureExt as _;
use gpui::{App, Context, Entity};
use rho_agent_hosts::HostId;
use rho_agent_types::MessageId;
use rho_agents_client::create::{AUTO_BASE_REV, Creation, Start};
use rho_agents_client::snippets::{DEFAULT_FUNCTION, Function, RUNTIMES, Runtime};
use rho_chat::ThreadId;
use rho_frame::{Candidate, Frame, Row, Rows};
use rho_window::style::StyleClass;

use super::AgentsSystem;

/// Everything a start needs but the message it starts from.
pub(crate) struct Plan {
    pub host: HostId,
    pub start: Start,
    pub runtime: Runtime,
}

/// Which message the agent starts from.
#[derive(Clone, Copy)]
pub(crate) enum Origin {
    /// The thread's message under the point: it exists, so the start goes
    /// out at once.
    Message,
    /// The composer's draft: sent first, started once the host logs it.
    Draft,
}

/// The `start agent` menu for `thread`: where the agent works. `read` is
/// the system as the caller holds it, leased, so it is not read through
/// `system` here.
pub(crate) fn rows(
    read: &AgentsSystem,
    system: Entity<AgentsSystem>,
    thread: ThreadId,
    origin: Origin,
    cx: &App,
) -> Rows {
    let mut rows = Rows::new("start agent");
    let on_repo = system.clone();
    rows.push(Row::run(
        "n",
        "new workset on a repository…",
        move |frame, window, cx| {
            let system = on_repo.clone();
            frame.open_prompt(
                "repository (url or host path):",
                Rc::new(|_, _, _| Vec::new()),
                Rc::new(move |frame, input, window, cx| {
                    let repo = input.trim();
                    if repo.is_empty() {
                        frame.echo("start agent: no repository", StyleClass::SystemInfo, cx);
                        return;
                    }
                    let start = Start::NewOn {
                        repo: Utf8PathBuf::from(repo),
                        rev: AUTO_BASE_REV.to_owned(),
                    };
                    prompt_runtime(system.clone(), thread, origin, start, frame, window, cx);
                }),
                window,
                cx,
            );
        },
    ));
    let here = system.clone();
    rows.push(Row::run(
        "h",
        "here, in a directory on the host…",
        move |frame, window, cx| {
            let system = here.clone();
            frame.open_prompt(
                "directory on the host:",
                Rc::new(|_, _, _| Vec::new()),
                Rc::new(move |frame, input, window, cx| {
                    let cwd = input.trim();
                    if cwd.is_empty() {
                        frame.echo("start agent: no directory", StyleClass::SystemInfo, cx);
                        return;
                    }
                    let start = Start::Here {
                        cwd: Utf8PathBuf::from(cwd),
                    };
                    prompt_runtime(system.clone(), thread, origin, start, frame, window, cx);
                }),
                window,
                cx,
            );
        },
    ));
    // Beside an agent of the thread, in its workset: the ones already
    // reading it first, then the rest of the host's.
    let registry = read.registry();
    let Some(host) = read.host_of_seed(thread.machine_seed) else {
        return rows;
    };
    let members = read
        .chat
        .read(cx)
        .chat(thread.machine_seed)
        .map(|chat| chat.members_under(Some(thread.message)))
        .unwrap_or_default();
    let mut beside: Vec<_> = members
        .iter()
        .copied()
        .filter(|agent| registry.host_of_agent(*agent) == Some(host))
        .collect();
    beside.extend(
        registry
            .host_agents(host)
            .into_iter()
            .filter(|agent| !members.contains(agent)),
    );
    for (index, agent) in beside.into_iter().take(9).enumerate() {
        let (Some(workset), Some(cwd)) = (
            registry.agent_workset(agent),
            registry.working_directory(agent),
        ) else {
            continue;
        };
        let system = system.clone();
        rows.push(Row::run(
            (index + 1).to_string(),
            format!("join {}", registry.agent_display_label(agent)),
            move |frame, window, cx| {
                let start = Start::Join {
                    workset,
                    cwd: cwd.clone(),
                };
                prompt_runtime(system.clone(), thread, origin, start, frame, window, cx);
            },
        ));
    }
    rows
}

fn prompt_runtime(
    system: Entity<AgentsSystem>,
    thread: ThreadId,
    origin: Origin,
    start: Start,
    frame: &mut Frame,
    window: &mut gpui::Window,
    cx: &mut Context<Frame>,
) {
    frame.open_prompt(
        format!("runtime ({}):", RUNTIMES[0]),
        Rc::new(|_, needle, _| {
            RUNTIMES
                .iter()
                .filter(|runtime| runtime.contains(needle))
                .map(|runtime| Candidate {
                    value: (*runtime).to_owned(),
                    description: "runtime".to_owned(),
                })
                .collect()
        }),
        Rc::new(move |frame, input, _, cx| {
            let runtime = match Runtime::parse(&input) {
                Ok(runtime) => runtime,
                Err(error) => {
                    frame.echo(&error, StyleClass::SystemInfo, cx);
                    return;
                }
            };
            let start = start.clone();
            system.update(cx, |system, cx| {
                let Some(host) = system.host_of_seed(thread.machine_seed) else {
                    system.frame.echo(
                        "the thread's host is not attached",
                        StyleClass::SystemInfo,
                        cx,
                    );
                    return;
                };
                let plan = Plan {
                    host,
                    start,
                    runtime,
                };
                match origin {
                    Origin::Message => create(system, plan, thread.message, cx),
                    Origin::Draft => {
                        let seed = thread.machine_seed;
                        let sent = system.chat.update(cx, |chat, cx| {
                            chat.send_draft(seed, Some(thread.message), cx)
                        });
                        match sent {
                            Some(client_id) => {
                                system.starting.insert((seed, client_id), plan);
                            }
                            None => system.frame.echo(
                                "send and start: nothing written",
                                StyleClass::SystemInfo,
                                cx,
                            ),
                        }
                    }
                }
            });
        }),
        window,
        cx,
    );
}

/// Sends the code that starts the agent from `origin`. The directory
/// shows the agent when it arrives; a refusal is echoed.
pub(crate) fn create(
    system: &mut AgentsSystem,
    plan: Plan,
    origin: MessageId,
    cx: &mut Context<AgentsSystem>,
) {
    let Some(agents) = system.agents_on(plan.host) else {
        system
            .frame
            .echo("the host is not attached", StyleClass::SystemInfo, cx);
        return;
    };
    let function = Function::parse(DEFAULT_FUNCTION).expect("the default function parses");
    let reply = match plan.start.creation(&function, &plan.runtime, Some(origin)) {
        Creation::Workset(create) => {
            let reply = agents.call(create);
            async move { reply.await.map(|created| created.result) }.boxed_local()
        }
        Creation::Eval(eval) => agents.call(eval).boxed_local(),
    };
    let frame = system.frame.clone();
    cx.spawn(async move |_, cx| {
        let reply = reply.await.and_then(|result| {
            if let Some(error) = result.error {
                anyhow::bail!("{error}");
            }
            result
                .agent
                .ok_or_else(|| anyhow::anyhow!("the creation code made no agent"))
        });
        cx.update(|cx| match reply {
            Ok(agent) => frame.echo(
                format!("agent {} started", agent.encoded()),
                StyleClass::SystemInfo,
                cx,
            ),
            Err(error) => frame.echo(
                format!("start agent: {error:#}"),
                StyleClass::SystemImportant,
                cx,
            ),
        });
    })
    .detach();
}
