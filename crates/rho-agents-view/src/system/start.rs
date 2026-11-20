//! Starting an agent from a mail.
//!
//! An agent can start from any mail: the one under the point, or
//! the one the composer is about to send. An idea sent in a project asks
//! nothing it need not: it works where the project's agents work, on the
//! default runtime, and only a project no agent started from yet asks
//! where, once. The rare `start agent` asks both:
//! where (a new workset on a repository, a directory on the host as it is, or
//! beside an agent already there) and the runtime. The GUI sends the code,
//! which names the mail as `origin`, so policy joins the agent to its thread
//! and hands it the mail first.

use std::collections::BTreeSet;
use std::rc::Rc;

use camino::Utf8PathBuf;
use gpui::{App, Context, Entity};
use rho_agent_hosts::HostId;
use rho_agents_client::create::{AUTO_BASE_REV, Creation, Start, project_start};
use rho_agents_client::protocol::worksets::{MailLaunch, StartFromMail};
use rho_agents_client::snippets::{DEFAULT_FUNCTION, DEFAULT_RUNTIME, Function, RUNTIMES, Runtime};
use rho_frame::{Candidate, Frame, Row, Rows};
use rho_mail::ThreadId;
use rho_mail::ui::IdeaSource;
use rho_window::style::StyleClass;

use super::AgentsSystem;

/// Everything a start needs but the mail it starts from.
pub(crate) struct Plan {
    pub host: HostId,
    pub start: Start,
    pub runtime: Runtime,
    feedback: Feedback,
}

/// The idea box already acknowledges posting; successful creation must
/// not replace that receipt. Failures remain visible in both flows.
enum Feedback {
    Started,
    Sent,
}

/// Which mail the agent starts from.
#[derive(Clone)]
pub(crate) enum Origin {
    /// The mail under the point: it exists, so the start goes
    /// out at once.
    Mail,
    /// The composer's draft: sent first, started once the host logs it.
    Draft(IdeaSource),
}

impl Origin {
    fn feedback(&self) -> Feedback {
        match self {
            Self::Draft(IdeaSource::Box(_)) => Feedback::Sent,
            _ => Feedback::Started,
        }
    }
}

/// What a start asks once it knows where.
#[derive(Clone, Copy)]
pub(crate) enum Runtimes {
    /// `runtime (default):`, for the rare start that asks everything.
    Ask,
    /// Nothing: the default runtime, for an idea sent in a project.
    Default,
}

/// Send an idea to the project rooted at `thread` and start an agent where
/// its agents already work, on the default runtime. If none has started yet,
/// ask where once; the project remembers where that agent works.
pub(crate) fn idea(
    system: &mut AgentsSystem,
    thread: ThreadId,
    source: IdeaSource,
    cx: &mut Context<AgentsSystem>,
) {
    let Some(host) = system.host_of(thread.host) else {
        system.frame.echo(
            "the thread's host is not attached",
            StyleClass::SystemInfo,
            cx,
        );
        return;
    };
    let registry = system.registry();
    let mail = system.mail.read(cx);
    let start = mail.host_mail(thread.host).and_then(|log| {
        let root = log.root_of(thread.message);
        let agents = registry
            .host_agents(host)
            .into_iter()
            .filter_map(|agent| registry.agent_identity(agent));
        project_start(agents, |message| {
            log.message(message).is_some() && log.root_of(message) == root
        })
    });
    // Before the project has an agent, which is also after its first start
    // failed, the answer the user gave is where.
    let start = start.or_else(|| system.places.get(thread.host, thread.message));
    match start {
        Some(start) => {
            let runtime = Runtime::parse(DEFAULT_RUNTIME).expect("the default runtime parses");
            let origin = Origin::Draft(source);
            send_and_start(
                system,
                thread,
                &origin,
                Plan {
                    host,
                    start,
                    runtime,
                    feedback: origin.feedback(),
                },
                cx,
            );
        }
        None => {
            let rows = rows(
                system,
                cx.entity(),
                thread,
                Origin::Draft(source),
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
        Runtimes::Ask => match origin {
            Origin::Mail => "start agent",
            Origin::Draft(_) => "send and start agent with options",
        },
        Runtimes::Default => "where this project's agents work",
    });
    let host = read.host_of(thread.host);
    let known = host
        .map(|host| known_places(read, host))
        .unwrap_or_default();
    let repositories = Rc::new(known.repositories);
    let directories = Rc::new(known.directories);
    let on_repo = system.clone();
    let repo_origin = origin.clone();
    rows.push(Row::run(
        "n",
        "new workset on a repository",
        move |frame, window, cx| {
            let system = on_repo.clone();
            let origin = repo_origin.clone();
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
                        origin.clone(),
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
    let here_origin = origin.clone();
    rows.push(Row::run(
        "h",
        "here, in a directory on the host",
        move |frame, window, cx| {
            let system = here.clone();
            let origin = here_origin.clone();
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
                        origin.clone(),
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
    // Join a destination of the work under the point, not just any
    // destination the host happens to know.
    let registry = read.registry();
    let Some(host) = host else {
        return rows;
    };
    let mail = read.mail.read(cx);
    let Some(log) = mail.host_mail(thread.host) else {
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
        let origin = origin.clone();
        rows.push(Row::run(
            (index + 1).to_string(),
            format!("join {subject}  {address}"),
            move |frame, window, cx| {
                let start = Start::Join {
                    workset,
                    cwd: cwd.clone(),
                };
                then(
                    system.clone(),
                    thread,
                    origin.clone(),
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
            let runtime = match Runtime::parse(&input) {
                Ok(runtime) => runtime,
                Err(error) => {
                    frame.echo(&error, StyleClass::SystemInfo, cx);
                    return;
                }
            };
            begin(
                system.clone(),
                thread,
                origin.clone(),
                start.clone(),
                runtime,
                cx,
            );
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
            feedback: origin.feedback(),
        };
        match origin {
            Origin::Mail => create(system, plan, thread, cx),
            Origin::Draft(_) => send_and_start(system, thread, &origin, plan, cx),
        }
    });
}

/// Sends the draft under `thread`; the agent starts once the host logs it.
fn send_and_start(
    system: &mut AgentsSystem,
    thread: ThreadId,
    origin: &Origin,
    plan: Plan,
    cx: &mut Context<AgentsSystem>,
) {
    let endpoint = thread.host;
    let sent = system.mail.update(cx, |mail, cx| match origin {
        Origin::Draft(source) => mail.send_idea(thread, source, cx),
        Origin::Mail => unreachable!("the origin already exists"),
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
    let origin = system
        .mail
        .read(cx)
        .host_mail(thread.host)
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
        Start::NewOn { repo, .. } => repo.to_string(),
        Start::Join { cwd, .. } | Start::Here { cwd } => cwd.to_string(),
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
                if matches!(plan.feedback, Feedback::Started) {
                    frame.echo(
                        format!("agent {} started · {on}", agent.encoded()),
                        StyleClass::SystemInfo,
                        cx,
                    );
                }
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
            }
        });
    })
    .detach();
}
