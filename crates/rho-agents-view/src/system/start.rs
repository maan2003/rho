//! Starting an agent from a mail.
//!
//! The rare start with options asks for a place and runtime. An ordinary
//! idea is routed entirely on its project's host, without this menu.

use std::collections::BTreeSet;
use std::rc::Rc;

use camino::Utf8PathBuf;
use gpui::{App, Context, Entity};
use rho_agent_hosts::HostId;
use rho_agents_client::create::{AUTO_BASE_REV, Creation, Start};
use rho_agents_client::protocol::worksets::{MailLaunch, StartFromMail};
use rho_agents_client::snippets::{
    self, DEFAULT_FUNCTION, DEFAULT_RUNTIME, Function, RUNTIMES, Runtime,
};
use rho_chat::ThreadId;
use rho_frame::{Candidate, Frame, Row, Rows};
use rho_window::style::StyleClass;

use super::AgentsSystem;

/// Everything a start needs but the mail it starts from.
pub(crate) struct Plan {
    pub host: HostId,
    pub start: Start,
    pub runtime: Runtime,
}

#[derive(Clone, Copy)]
pub(crate) enum Origin {
    Mail,
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
    let mut rows = Rows::new(match origin {
        Origin::Mail => "start agent",
        Origin::Draft => "send and start agent with options",
    });
    let host = read.host_of(thread.host);
    let known = host
        .map(|host| known_places(read, host))
        .unwrap_or_default();
    let repositories = Rc::new(known.repositories);
    let directories = Rc::new(known.directories);
    let on_repo = system.clone();
    let repo_origin = origin;
    rows.push(Row::run(
        "n",
        "new workset on a repository",
        move |frame, window, cx| {
            let system = on_repo.clone();
            let origin = repo_origin;
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
                    prompt_runtime(system.clone(), thread, origin, start, frame, window, cx);
                }),
                window,
                cx,
            );
        },
    ));
    let here = system.clone();
    let here_origin = origin;
    rows.push(Row::run(
        "h",
        "here, in a directory on the host",
        move |frame, window, cx| {
            let system = here.clone();
            let origin = here_origin;
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
                    prompt_runtime(system.clone(), thread, origin, start, frame, window, cx);
                }),
                window,
                cx,
            );
        },
    ));
    // Join a destination of the work under the point, not just any
    // destination the host happens to know.
    let registry = read.registry();
    let Some(host) = host else {
        return rows;
    };
    let mail = read.mail.read(cx);
    let Some(log) = mail.host_chat(thread.host) else {
        return rows;
    };
    let members = if log.is_project(thread.message) {
        log.members_in_project(thread.message).into_iter().collect()
    } else {
        log.members_under(Some(thread.message))
    };
    let mut seen = BTreeSet::new();
    let destinations = members
        .into_iter()
        .filter(|&agent| registry.host_of_agent(agent) == Some(host))
        .filter_map(|agent| {
            let work = ThreadId {
                host: thread.host,
                message: log.agent_thread(agent)?,
            };
            let (subject, address) = mail.thread_name(work, cx)?;
            let workset = registry.agent_workset(agent)?;
            let cwd = registry.working_directory(agent)?;
            Some((workset, cwd, subject, address))
        })
        .filter(|(workset, cwd, _, _)| seen.insert((*workset, cwd.clone())));
    for (index, (workset, cwd, subject, address)) in destinations.take(9).enumerate() {
        let system = system.clone();
        let origin = origin;
        rows.push(Row::run(
            (index + 1).to_string(),
            format!("join {subject}  {address}"),
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

pub(crate) fn known_project_places(read: &AgentsSystem, host: HostId) -> Vec<Candidate> {
    known_places(read, host).repositories
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
                    description: if *runtime == DEFAULT_RUNTIME {
                        "default"
                    } else {
                        ""
                    }
                    .to_owned(),
                })
                .collect()
        }),
        Rc::new(move |frame, input, _, cx| {
            let runtime = match snippets::parse_runtime(&input) {
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
            Origin::Mail => create(system, plan, thread, cx),
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
    let sent = system
        .mail
        .update(cx, |mail, cx| mail.post_draft_with_options(thread, cx));
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
    let origin = system
        .mail
        .read(cx)
        .host_chat(thread.host)
        .and_then(|chat| chat.message(thread.message))
        .filter(|mail| mail.from == rho_agent_types::Address::StartFailed)
        .and_then(|mail| mail.reply_to)
        .unwrap_or(thread.message);
    let Some(agents) = system.agents_on(plan.host) else {
        system
            .frame
            .echo("the host is not attached", StyleClass::SystemInfo, cx);
        return;
    };
    let function = Function::parse(DEFAULT_FUNCTION).expect("the default function parses");
    // Where and on what, said back: an idea's start asked neither.
    let at = match &plan.start {
        Start::NewOn { repo, .. } => repo
            .as_str()
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .map(|tail| tail.strip_suffix(".git").unwrap_or(tail))
            .unwrap_or(repo.as_str()),
        Start::Join { cwd, .. } | Start::Here { cwd } => cwd.file_name().unwrap_or(cwd.as_str()),
    };
    let on = format!("{} · {at}", plan.runtime.label());
    let launch = match plan.start.creation(&function, &plan.runtime, Some(origin)) {
        Creation::Workset(create) => MailLaunch::New(create),
        Creation::Eval(eval) => MailLaunch::Existing(eval),
    };
    let reply = agents.call(StartFromMail {
        origin,
        launch,
        host_label: system.registry.host_name(plan.host).to_owned(),
    });
    let frame = system.frame.clone();
    cx.spawn(async move |this, cx| {
        let reply = reply.await;
        cx.update(|cx| match reply {
            Ok(agent) => {
                let _ = this.update(cx, |system, cx| {
                    let label = system
                        .registry()
                        .agent_id_label_on_host(agent, Some(plan.host));
                    frame.echo(
                        format!("started {label} · {on}"),
                        StyleClass::SystemInfo,
                        cx,
                    );
                    // Its first turn may still fail, such as for want of a
                    // login on the host; the echo line says so when it does.
                    system.watch_first_turn(agent, cx);
                });
            }
            Err(error) => {
                let reason = format!("{error:#}");
                frame.echo(
                    format!("agent did not start: {reason}"),
                    StyleClass::SystemImportant,
                    cx,
                );
            }
        });
    })
    .detach();
}
