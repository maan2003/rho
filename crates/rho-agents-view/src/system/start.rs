//! Starting an agent from a message.
//!
//! Any message is what an agent starts from: the one under the point, or
//! the one the composer is about to send. An idea sent in a channel asks
//! nothing it need not: it works where the channel's agents work, on the
//! default runtime, and only a channel no agent started from yet asks
//! where, once. The rare `start agent, choosing where and runtime…` asks both:
//! where (a new workset on a repository, a directory on the host as it is, or
//! beside an agent already there) and the runtime. The GUI sends the code,
//! which names the message as `origin`, so policy joins the agent to its thread
//! and hands it the message first.

use std::rc::Rc;

use camino::Utf8PathBuf;
use futures::FutureExt as _;
use gpui::{App, Context, Entity};
use rho_agent_hosts::HostId;
use rho_agents_client::create::{AUTO_BASE_REV, Creation, Start, channel_start};
use rho_agents_client::snippets::{DEFAULT_FUNCTION, DEFAULT_RUNTIME, Function, RUNTIMES, Runtime};
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

/// What a start asks once it knows where.
#[derive(Clone, Copy)]
pub(crate) enum Runtimes {
    /// `runtime (default):`, for the rare start that asks everything.
    Ask,
    /// Nothing: the default runtime, for an idea sent in a channel.
    Default,
}

/// An idea sent in the channel `thread`: sent and started where the
/// channel's agents work, on the default runtime. A channel no agent has
/// started from asks where, once; the agent it starts is what the channel
/// remembers.
pub(crate) fn idea(system: &mut AgentsSystem, thread: ThreadId, cx: &mut Context<AgentsSystem>) {
    let Some(host) = system.host_of(thread.host) else {
        system.frame.echo(
            "the thread's host is not attached",
            StyleClass::SystemInfo,
            cx,
        );
        return;
    };
    let registry = system.registry();
    let chat = system.chat.read(cx);
    let start = chat.chat(thread.host).and_then(|chat| {
        let channel = chat.root_of(thread.message);
        let agents = registry
            .host_agents(host)
            .into_iter()
            .filter_map(|agent| registry.agent_identity(agent));
        channel_start(agents, |message| {
            chat.message(message).is_some() && chat.root_of(message) == channel
        })
    });
    // Before the channel has an agent, which is also after its first start
    // failed, the answer the user gave is where.
    let start = start.or_else(|| system.places.get(thread.host, thread.message));
    match start {
        Some(start) => {
            let runtime = Runtime::parse(DEFAULT_RUNTIME).expect("the default runtime parses");
            send_and_start(
                system,
                thread,
                Plan {
                    host,
                    start,
                    runtime,
                },
                cx,
            );
        }
        None => {
            let rows = rows(
                system,
                cx.entity(),
                thread,
                Origin::Draft,
                Runtimes::Default,
                cx,
            );
            system.frame.menu(rows, cx);
        }
    }
}

/// The `start agent` menu for `thread`: where the agent works. `read` is
/// the system as the caller holds it, leased, so it is not read through
/// `system` here.
pub(crate) fn rows(
    read: &AgentsSystem,
    system: Entity<AgentsSystem>,
    thread: ThreadId,
    origin: Origin,
    runtimes: Runtimes,
    cx: &App,
) -> Rows {
    // The question an idea asks once: its answer is where the channel's
    // agents work from then on.
    let mut rows = Rows::new(match runtimes {
        Runtimes::Ask => "start agent",
        Runtimes::Default => "where this channel's agents work",
    });
    let host = read.host_of(thread.host);
    let known = host
        .map(|host| known_places(read, host))
        .unwrap_or_default();
    let repositories = Rc::new(known.repositories);
    let directories = Rc::new(known.directories);
    let on_repo = system.clone();
    rows.push(Row::run(
        "n",
        "new workset on a repository…",
        move |frame, window, cx| {
            let system = on_repo.clone();
            frame.open_prompt(
                "repository (url or host path):",
                completions(repositories.clone()),
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
                    then(
                        system.clone(),
                        thread,
                        origin,
                        runtimes,
                        start,
                        frame,
                        window,
                        cx,
                    );
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
                completions(directories.clone()),
                Rc::new(move |frame, input, window, cx| {
                    let cwd = input.trim();
                    if cwd.is_empty() {
                        frame.echo("start agent: no directory", StyleClass::SystemInfo, cx);
                        return;
                    }
                    let start = Start::Here {
                        cwd: Utf8PathBuf::from(cwd),
                    };
                    then(
                        system.clone(),
                        thread,
                        origin,
                        runtimes,
                        start,
                        frame,
                        window,
                        cx,
                    );
                }),
                window,
                cx,
            );
        },
    ));
    // Beside an agent of the thread, in its workset: the ones already
    // reading it first, then the rest of the host's.
    let registry = read.registry();
    let Some(host) = host else {
        return rows;
    };
    let members = read
        .chat
        .read(cx)
        .chat(thread.host)
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
                then(
                    system.clone(),
                    thread,
                    origin,
                    runtimes,
                    start,
                    frame,
                    window,
                    cx,
                );
            },
        ));
    }
    rows
}

/// What the host already knows of where agents work, newest first, each
/// once with what it is: the repositories its worksets were made on, and
/// the directories on it its agents work in as they are.
#[derive(Default)]
struct KnownPlaces {
    /// Repositories, then host directories: either answers
    /// `repository (url or host path):`.
    repositories: Vec<Candidate>,
    directories: Vec<Candidate>,
}

fn known_places(read: &AgentsSystem, host: HostId) -> KnownPlaces {
    let registry = read.registry();
    let mut agents: Vec<_> = registry
        .host_agents(host)
        .into_iter()
        .filter_map(|agent| registry.agent_identity(agent))
        .collect();
    agents.sort_by_key(|agent| std::cmp::Reverse(agent.entry.created_at));
    let mut known = KnownPlaces::default();
    let add = |into: &mut Vec<Candidate>, value: &str, description: &str| {
        if !into.iter().any(|known| known.value == value) {
            into.push(Candidate {
                value: value.to_owned(),
                description: description.to_owned(),
            });
        }
    };
    for agent in &agents {
        if let Some(repo) = agent.repository() {
            add(&mut known.repositories, repo, "repository");
        }
    }
    for agent in &agents {
        let place = agent.place();
        if place.workset.is_none() {
            add(
                &mut known.repositories,
                place.cwd.as_str(),
                "directory on the host",
            );
            add(
                &mut known.directories,
                place.cwd.as_str(),
                "directory on the host",
            );
        }
    }
    known
}

/// A prompt's answers: the known ones that hold what is typed.
fn completions(known: Rc<Vec<Candidate>>) -> rho_frame::CandidateSource {
    Rc::new(move |_, needle, _| {
        let needle = needle.trim();
        known
            .iter()
            .filter(|candidate| candidate.value.contains(needle))
            .cloned()
            .collect()
    })
}

/// Where is answered: ask the runtime, or start on the default.
#[allow(clippy::too_many_arguments)]
fn then(
    system: Entity<AgentsSystem>,
    thread: ThreadId,
    origin: Origin,
    runtimes: Runtimes,
    start: Start,
    frame: &mut Frame,
    window: &mut gpui::Window,
    cx: &mut Context<Frame>,
) {
    match runtimes {
        Runtimes::Ask => prompt_runtime(system, thread, origin, start, frame, window, cx),
        Runtimes::Default => {
            // The channel's question, answered: kept, so the next idea asks
            // nothing even if this start fails.
            system.update(cx, |system, _| {
                system.places.remember(thread.host, thread.message, &start)
            });
            let runtime = Runtime::parse(DEFAULT_RUNTIME).expect("the default runtime parses");
            begin(system, thread, origin, start, runtime, cx);
        }
    }
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
        format!("runtime ({DEFAULT_RUNTIME}):"),
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
            begin(system.clone(), thread, origin, start.clone(), runtime, cx);
        }),
        window,
        cx,
    );
}

/// Starts the agent from `origin` once where and the runtime are known.
fn begin(
    system: Entity<AgentsSystem>,
    thread: ThreadId,
    origin: Origin,
    start: Start,
    runtime: Runtime,
    cx: &mut App,
) {
    system.update(cx, |system, cx| {
        let Some(host) = system.host_of(thread.host) else {
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
            Origin::Message => create(system, plan, thread, cx),
            Origin::Draft => send_and_start(system, thread, plan, cx),
        }
    });
}

/// Sends the draft under `thread`; the agent starts once the host logs it.
fn send_and_start(
    system: &mut AgentsSystem,
    thread: ThreadId,
    plan: Plan,
    cx: &mut Context<AgentsSystem>,
) {
    let endpoint = thread.host;
    let sent = system.chat.update(cx, |chat, cx| {
        chat.send_draft(endpoint, Some(thread.message), cx)
    });
    match sent {
        Some(client_id) => {
            system.starting.insert((endpoint, client_id), plan);
        }
        None => system.frame.echo(
            "send and start: nothing written",
            StyleClass::SystemInfo,
            cx,
        ),
    }
}

/// Sends the code that starts the agent from `origin`. The directory
/// shows the agent when it arrives; a refusal is echoed.
pub(crate) fn create(
    system: &mut AgentsSystem,
    plan: Plan,
    thread: ThreadId,
    cx: &mut Context<AgentsSystem>,
) {
    let origin = thread.message;
    let Some(agents) = system.agents_on(plan.host) else {
        system
            .frame
            .echo("the host is not attached", StyleClass::SystemInfo, cx);
        return;
    };
    let function = Function::parse(DEFAULT_FUNCTION).expect("the default function parses");
    // Where and on what, said back: an idea's start asked neither.
    let at = match &plan.start {
        Start::NewOn { repo, .. } => repo.to_string(),
        Start::Join { cwd, .. } | Start::Here { cwd } => cwd.to_string(),
    };
    let on = format!("{} · {at}", plan.runtime.label());
    let reply = match plan.start.creation(&function, &plan.runtime, Some(origin)) {
        Creation::Workset(create) => {
            let reply = agents.call(create);
            async move { reply.await.map(|created| created.result) }.boxed_local()
        }
        Creation::Eval(eval) => agents.call(eval).boxed_local(),
    };
    let frame = system.frame.clone();
    cx.spawn(async move |this, cx| {
        let reply = reply.await.and_then(|result| {
            if let Some(error) = result.error {
                anyhow::bail!("{error}");
            }
            result
                .agent
                .ok_or_else(|| anyhow::anyhow!("the creation code made no agent"))
        });
        cx.update(|cx| match reply {
            Ok(agent) => {
                frame.echo(
                    format!("agent {} started · {on}", agent.encoded()),
                    StyleClass::SystemInfo,
                    cx,
                );
                // Its first turn may still fail, such as for want of a
                // login on the host; the echo line says so when it does.
                let _ = this.update(cx, |system, cx| system.watch_first_turn(agent, cx));
            }
            Err(error) => {
                let reason = format!("{error:#}");
                frame.echo(
                    format!("agent did not start: {reason}"),
                    StyleClass::SystemImportant,
                    cx,
                );
                let _ = this.update(cx, |system, cx| {
                    system
                        .chat
                        .update(cx, |chat, cx| chat.note_failed_start(thread, reason, cx))
                });
            }
        });
    })
    .detach();
}
