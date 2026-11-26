//! The commands about agents and their work, on every screen
//! (`window.commands are global in the palette`). A command acts on what
//! the screen in view is about when that is one agent or one workset, and
//! asks which otherwise.

use std::rc::Rc;

use gpui::{App, Context, Entity, Window};
use rho_agent_types::{Address as Party, AgentId, Message};
use rho_chat::ui::Event as MailEvent;
use rho_frame::{Addressed as _, Candidate, Frame, Point, Row, Subject};
use rho_terminal::protocol::TerminalInfo;
use rho_window::style::StyleClass;

use super::{AgentAddress, AgentsSystem, Scope, steer};

/// What a command may mean, as the screen in view says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    /// One, unambiguously: the command acts on it.
    One(AgentId),
    /// Several the screen is about: the command asks among them.
    Among(Vec<AgentId>),
    /// The screen says nothing: the command asks among all.
    Anything,
}

impl Target {
    fn of(mut agents: Vec<AgentId>) -> Self {
        agents.dedup();
        match agents.as_slice() {
            [] => Self::Anything,
            [agent] => Self::One(*agent),
            _ => Self::Among(agents),
        }
    }
}

/// The step that wrote a message the screen shows: what `look into the
/// work` opens.
#[derive(Clone)]
pub(super) struct Written {
    pub agent: AgentId,
    pub mail: Message,
    /// The message is the one under the point, where `enter` opens it too.
    pub under_point: bool,
}

/// What the screen in view is about, for the commands.
#[derive(Clone)]
pub(super) struct Here {
    /// The agents a steering command means.
    pub steer: Target,
    /// The worksets a command about the work means, each by the agent its
    /// new tools start under.
    pub work: Target,
    pub written: Option<Written>,
}

/// What the screen in view is about, read from the agents the screen
/// names: a transcript's, the owner of a tool, or, in a thread, the
/// draft's worker, the writer of the message under the point, or the
/// thread's members. `leads` maps an agent to its workset's lead.
pub(super) fn read_here(
    scope: Option<(Scope, Vec<AgentId>)>,
    tool_owner: Option<AgentId>,
    subject: &Subject,
    written: Option<Written>,
    members: Vec<AgentId>,
    lead: impl Fn(AgentId) -> AgentId,
) -> Here {
    if let Some((scope, agents)) = scope {
        let work = match scope {
            Scope::Agent(agent) => Target::One(agent),
            Scope::Workset(_) => Target::of(agents.first().copied().into_iter().collect()),
        };
        return Here {
            steer: Target::of(agents),
            work,
            written: None,
        };
    }
    if let Some(agent) = tool_owner {
        return Here {
            steer: Target::One(agent),
            work: Target::One(agent),
            written: None,
        };
    }
    let steer = match (subject, &written) {
        (
            Subject::Draft {
                worker: Some(agent),
            },
            _,
        ) => Target::One(*agent),
        (Subject::Draft { worker: None }, _) => Target::of(members),
        (_, Some(written)) if written.under_point => Target::One(written.agent),
        _ => Target::of(members),
    };
    let work = match &steer {
        Target::One(agent) => Target::One(lead(*agent)),
        Target::Among(agents) => {
            let mut leads = Vec::new();
            for agent in agents {
                let lead = lead(*agent);
                if !leads.contains(&lead) {
                    leads.push(lead);
                }
            }
            Target::of(leads)
        }
        Target::Anything => Target::Anything,
    };
    Here {
        steer,
        work,
        written,
    }
}

impl AgentsSystem {
    /// What the screen in view is about.
    pub(super) fn here(&self, point: &Point, cx: &App) -> Here {
        let known = |agent: &AgentId| self.registry.mirrored(*agent).is_some();
        let scope = Scope::from_address(&point.surface)
            .and_then(|scope| Some((scope, self.scope_agents(scope)?.1)));
        let tool_owner = AgentAddress::from_address(&point.surface)
            .map(|at| at.agent)
            .filter(known);
        let subject = match &point.subject {
            Subject::Draft {
                worker: Some(worker),
            } if !known(worker) => Subject::Draft { worker: None },
            subject => subject.clone(),
        };
        let members = self
            .thread_at(point, cx)
            .and_then(|thread| {
                let chat = self.mail.read(cx);
                Some(
                    chat.host_chat(thread.host)?
                        .members_under(Some(thread.message)),
                )
            })
            .unwrap_or_default()
            .into_iter()
            .filter(known)
            .collect();
        read_here(
            scope,
            tool_owner,
            &subject,
            self.written_at(point, cx),
            members,
            |agent| self.lead_of(agent).unwrap_or(agent),
        )
    }

    /// The step that wrote the message the point is on, in a thread; on
    /// the desk, the newest an agent wrote in the card under the point.
    fn written_at(&self, point: &Point, cx: &App) -> Option<Written> {
        let thread = self.thread_at(point, cx)?;
        let system = self.mail.read(cx);
        let chat = system.host_chat(thread.host)?;
        let author = |mail: &Message| match mail.from {
            Party::Agent(agent) | Party::System { agent, .. } => Some(agent),
            _ => None,
        };
        let under_point = system.thread_of(&point.surface).is_some();
        let mail = match (under_point, &point.subject) {
            (true, Subject::Draft { .. }) => return None,
            (true, _) => chat.message(thread.message)?,
            (false, _) => std::iter::once(thread.message)
                .chain(chat.children(Some(thread.message)).iter().copied())
                .filter_map(|id| chat.message(id))
                .filter(|mail| author(mail).is_some() && !chat.agent_only(mail.id))
                .max_by_key(|mail| mail.version().0)?,
        };
        let agent = author(mail).filter(|agent| self.registry.mirrored(*agent).is_some())?;
        Some(Written {
            agent,
            mail: mail.clone(),
            under_point,
        })
    }

    /// Every agent, the most recently active first.
    fn every_agent(&self) -> Vec<AgentId> {
        let mut agents: Vec<_> = self.registry.known_agents().copied().collect();
        agents.sort_by_key(|agent| {
            std::cmp::Reverse(self.registry.agent_last_active(*agent).map_or(0, |at| at.0))
        });
        agents
    }

    /// Every workset by its lead, the most recently active first.
    fn every_workset(&self) -> Vec<AgentId> {
        let mut leads = Vec::new();
        for agent in self.every_agent() {
            if let Some(lead) = self.lead_of(agent)
                && !leads.contains(&lead)
            {
                leads.push(lead);
            }
        }
        leads
    }

    /// An agent as a prompt offers it: its label, and where and how it is.
    fn agent_candidate(&self, agent: AgentId) -> Candidate {
        let (label, host) = self.label(agent);
        let mut description = vec![host];
        description.extend(
            self.registry
                .agent_spec(agent)
                .map(|spec| spec.runtime_label()),
        );
        description.extend(
            self.registry
                .agent_facts(agent)
                .runtime
                .map(|heard| heard.state.activity().word().to_owned()),
        );
        Candidate {
            value: label,
            description: description.join(" · "),
        }
    }

    /// A workset as a prompt offers it, by its lead: where it works, and
    /// how many agents work in it.
    fn workset_candidate(&self, lead: AgentId) -> Candidate {
        let (label, host) = self.label(lead);
        let mut description = vec![host];
        description.extend(
            self.registry
                .working_directory(lead)
                .map(|directory| directory.to_string()),
        );
        let agents = self
            .registry
            .agent_workset(lead)
            .map_or(1, |workset| self.workset_agents(workset).len());
        if agents > 1 {
            description.push(format!("{agents} agents"));
        }
        Candidate {
            value: label,
            description: description.join(" · "),
        }
    }
}

type Then =
    Rc<dyn Fn(&Entity<AgentsSystem>, AgentId, &mut Frame, &mut Window, &mut Context<Frame>)>;

/// What a prompt asks for.
#[derive(Clone, Copy)]
enum Ask {
    Agent,
    Workset,
}

/// Runs `then` on the target when it is one, and otherwise asks which.
fn choose(
    system: &Entity<AgentsSystem>,
    target: &Target,
    ask: Ask,
    then: Then,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let agents = match target {
        Target::One(agent) => return then(system, *agent, frame, window, cx),
        Target::Among(agents) => agents.clone(),
        Target::Anything => match ask {
            Ask::Agent => system.read(cx).every_agent(),
            Ask::Workset => system.read(cx).every_workset(),
        },
    };
    let choices: Rc<Vec<(AgentId, Candidate)>> = Rc::new({
        let system = system.read(cx);
        agents
            .into_iter()
            .map(|agent| {
                let candidate = match ask {
                    Ask::Agent => system.agent_candidate(agent),
                    Ask::Workset => system.workset_candidate(agent),
                };
                (agent, candidate)
            })
            .collect()
    });
    if choices.is_empty() {
        let words = match ask {
            Ask::Agent => "no agent",
            Ask::Workset => "no workset",
        };
        frame.echo(words, StyleClass::SystemInfo, cx);
        return;
    }
    let question = match ask {
        Ask::Agent => "agent:",
        Ask::Workset => "workset:",
    };
    let offered = choices.clone();
    let system = system.clone();
    frame.open_prompt(
        question,
        Rc::new(move |_, needle, _| {
            let needle = needle.trim().to_lowercase();
            offered
                .iter()
                .map(|(_, candidate)| candidate)
                .filter(|candidate| {
                    candidate.value.to_lowercase().contains(&needle)
                        || candidate.description.to_lowercase().contains(&needle)
                })
                .cloned()
                .collect()
        }),
        Rc::new(move |frame, input, window, cx| {
            let input = input.trim();
            match choices
                .iter()
                .find(|(agent, candidate)| candidate.value == input || agent.encoded() == input)
            {
                Some((agent, _)) => then(&system, *agent, frame, window, cx),
                None => frame.echo(
                    &format!("{question} none called {input:?}"),
                    StyleClass::SystemInfo,
                    cx,
                ),
            }
        }),
        window,
        cx,
    );
}

/// A row that acts on what the screen is about ranks with the screen's
/// commands; one that must ask ranks with the window's.
fn ranked(row: Row, target: &Target) -> Row {
    match target {
        Target::Anything => row.window_wide(),
        Target::One(_) | Target::Among(_) => row,
    }
}

/// Every command about agents and their work, each acting on `here`.
pub(super) fn rows(system: Entity<AgentsSystem>, here: Here) -> Vec<Row> {
    let mut rows = Vec::new();
    let steering = |name: &'static str, act: Then| {
        let system = system.clone();
        let target = here.steer.clone();
        ranked(
            Row::run("", name, move |frame, window, cx| {
                choose(&system, &target, Ask::Agent, act.clone(), frame, window, cx)
            }),
            &here.steer,
        )
    };
    let rewind_target = here.steer.clone();
    let rewind_system = system.clone();
    rows.push(ranked(
        Row::counted("", "rewind turns", move |frame, count, window, cx| {
            let turns = count.unwrap_or(1);
            choose(
                &rewind_system,
                &rewind_target,
                Ask::Agent,
                Rc::new(move |system, agent, _, _, cx| steer::rewind(system, agent, turns, cx)),
                frame,
                window,
                cx,
            )
        }),
        &here.steer,
    ));
    rows.push(steering(
        "compact context",
        Rc::new(|system, agent, _, _, cx| steer::compact(system, agent, cx)),
    ));
    rows.push(steering(
        "new cache key",
        Rc::new(|system, agent, _, _, cx| steer::new_cache_key(system, agent, cx)),
    ));
    rows.push(steering(
        "set runtime…",
        Rc::new(|system, agent, frame, window, cx| {
            steer::set_runtime(system, agent, frame, window, cx)
        }),
    ));
    rows.push(steering(
        "stop agent",
        Rc::new(|system, agent, _, _, cx| steer::stop(system, agent, cx)),
    ));
    rows.push(steering(
        "cancel the request",
        Rc::new(|system, agent, _, _, cx| steer::cancel(system, agent, cx)),
    ));

    let working = |name: &'static str, act: Then| {
        let system = system.clone();
        let target = here.work.clone();
        ranked(
            Row::run("", name, move |frame, window, cx| {
                choose(
                    &system,
                    &target,
                    Ask::Workset,
                    act.clone(),
                    frame,
                    window,
                    cx,
                )
            }),
            &here.work,
        )
    };
    rows.push(working(
        "shell",
        Rc::new(|system, agent, frame, window, cx| {
            if system.update(cx, |system, cx| system.attached(agent, cx)) {
                let card = frame.card_to_inspect(cx);
                frame.open_inspection(AgentAddress::shell(agent).address(), card, window, cx);
            }
        }),
    ));
    rows.push(working(
        "close shell",
        Rc::new(|system, agent, _, _, cx| {
            system.update(cx, |system, cx| system.close_shell(agent, cx));
        }),
    ));
    rows.push(working(
        "terminal",
        Rc::new(|system, agent, frame, _, cx| {
            let card = frame.card_to_inspect(cx);
            system.update(cx, |system, cx| {
                system.open_terminal(agent, false, card, cx)
            });
        }),
    ));
    rows.push(working(
        "new terminal",
        Rc::new(|system, agent, frame, _, cx| {
            let card = frame.card_to_inspect(cx);
            system.update(cx, |system, cx| system.open_terminal(agent, true, card, cx));
        }),
    ));
    rows.push(working(
        "desktop…",
        Rc::new(|system, agent, frame, _, cx| {
            let card = frame.card_to_inspect(cx);
            system.update(cx, |system, cx| system.open_desktop(agent, card, cx));
        }),
    ));
    rows.push(working(
        "open its thread",
        Rc::new(|system, agent, frame, window, cx| {
            let lead = system.read(cx).lead_of(agent).unwrap_or(agent);
            match system.read(cx).agent_thread(lead, cx) {
                Some(thread) => frame.open(thread.address(), window, cx),
                None => frame.echo("the work has no thread", StyleClass::SystemInfo, cx),
            }
        }),
    ));
    rows.push(working("filter by agent…", Rc::new(filter)));
    rows.push(match here.written {
        // The step that wrote the message in view.
        Some(Written {
            agent,
            mail,
            under_point,
        }) => {
            let system = system.clone();
            let row = Row::run("", "look into the work", move |_, _, cx| {
                let mail = mail.clone();
                system.update(cx, |system, cx| {
                    system
                        .mail
                        .update(cx, |_, cx| cx.emit(MailEvent::Inspect { agent, mail }))
                });
            });
            match under_point {
                true => row.bound_to(rho_frame::thread::OpenRow),
                false => row,
            }
        }
        None => working(
            "look into the work",
            Rc::new(|system, agent, frame, window, cx| {
                let Some(workset) = system.read(cx).registry().agent_workset(agent) else {
                    frame.echo("the agent is unavailable", StyleClass::SystemInfo, cx);
                    return;
                };
                let card = frame.card_to_inspect(cx);
                frame.open_inspection(Scope::Workset(workset).address(), card, window, cx);
            }),
        ),
    });
    rows
}

/// Narrows the workset's transcript to one of its agents, or widens it to
/// the whole workset again. A transcript of the workset in view switches
/// in place.
fn filter(
    system: &Entity<AgentsSystem>,
    agent: AgentId,
    frame: &mut Frame,
    window: &mut Window,
    cx: &mut Context<Frame>,
) {
    let Some(workset) = system.read(cx).registry().agent_workset(agent) else {
        frame.echo("the agent is unavailable", StyleClass::SystemInfo, cx);
        return;
    };
    const WHOLE: &str = "whole workset";
    let (choices, current) = {
        let system = system.read(cx);
        let mut choices = vec![(
            Scope::Workset(workset),
            Candidate {
                value: WHOLE.to_owned(),
                description: match system.workset_agents(workset).len() {
                    1 => "1 agent".to_owned(),
                    agents => format!("{agents} agents"),
                },
            },
        )];
        choices.extend(
            system
                .workset_agents(workset)
                .into_iter()
                .map(|agent| (Scope::Agent(agent), system.agent_candidate(agent))),
        );
        let current = Scope::from_address(frame.current_address())
            .filter(|scope| system.scope_workset(*scope) == Some(workset));
        (Rc::new(choices), current)
    };
    let shown = current.map_or(WHOLE.to_owned(), |current| {
        choices
            .iter()
            .find(|(scope, _)| *scope == current)
            .map_or(WHOLE.to_owned(), |(_, candidate)| candidate.value.clone())
    });
    let offered = choices.clone();
    frame.open_prompt(
        format!("filter ({shown}):"),
        Rc::new(move |_, needle, _| {
            let needle = needle.trim().to_lowercase();
            offered
                .iter()
                .map(|(_, candidate)| candidate)
                .filter(|candidate| candidate.value.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        }),
        Rc::new(move |frame, input, window, cx| {
            let input = input.trim();
            let Some((scope, _)) = choices.iter().find(|(_, choice)| choice.value == input) else {
                frame.echo(
                    &format!("filter: none called {input:?}"),
                    StyleClass::SystemInfo,
                    cx,
                );
                return;
            };
            match current {
                Some(_) => frame.replace_current(scope.address(), window, cx),
                None => {
                    let card = frame.card_to_inspect(cx);
                    frame.open_inspection(scope.address(), card, window, cx)
                }
            }
        }),
        window,
        cx,
    );
}

/// Which terminal `terminal` opens in a workset.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TerminalPick {
    /// Start the owner's terminal with this number, or attach to it.
    Start(u64),
    /// The one that runs.
    Open(AgentId, u64),
    /// Several run: the user says which.
    Ask(Vec<(AgentId, TerminalInfo)>),
}

/// What `terminal` (or `new terminal`, with `new`) opens for `owner`,
/// given the terminals that run in its workset and whose each is.
pub(super) fn pick_terminal(
    owner: AgentId,
    running: Vec<(AgentId, TerminalInfo)>,
    new: bool,
) -> TerminalPick {
    let next = || {
        let owned: Vec<TerminalInfo> = running
            .iter()
            .filter(|(agent, _)| *agent == owner)
            .map(|(_, info)| info.clone())
            .collect();
        rho_terminal::channel::next_terminal_id(&owned)
    };
    if new {
        return TerminalPick::Start(next());
    }
    match running.as_slice() {
        [] => TerminalPick::Start(0),
        [(agent, info)] => TerminalPick::Open(*agent, info.terminal_id),
        _ => TerminalPick::Ask(running),
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentIdDomain, ChatSeq, Message, MessageId, Revision, UnixMs};

    use super::*;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &AgentIdDomain(3)).unwrap()
    }

    fn running(agent: AgentId, terminal_id: u64) -> (AgentId, TerminalInfo) {
        (
            agent,
            TerminalInfo {
                agent: agent.encoded(),
                terminal_id,
                title: String::new(),
                cols: 80,
                rows: 24,
                clients: 0,
            },
        )
    }

    fn written(agent: AgentId, under_point: bool) -> Written {
        Written {
            agent,
            mail: Message {
                id: MessageId(ChatSeq(4)),
                at: UnixMs(1),
                parent: None,
                reply_to: None,
                thread_reply: None,
                from: Party::Agent(agent),
                kind: Default::default(),
                mentions: Vec::new(),
                content: Vec::new(),
                ping_started: None,
                revision: Revision::Original,
            },
            under_point,
        }
    }

    /// Every terminal that runs stays reachable: a second running one is
    /// never hidden behind the first.
    #[test]
    fn terminal_asks_when_more_than_one_runs() {
        let (lead, helper) = (agent(1), agent(2));
        assert_eq!(pick_terminal(lead, vec![], false), TerminalPick::Start(0));
        assert_eq!(
            pick_terminal(lead, vec![running(lead, 1)], false),
            TerminalPick::Open(lead, 1),
            "the one that runs, whatever its number"
        );
        let two = vec![running(lead, 0), running(helper, 0)];
        assert_eq!(
            pick_terminal(lead, two.clone(), false),
            TerminalPick::Ask(two),
            "a helper's terminal in the workset is as reachable as the lead's"
        );
        // A new one is numbered after the owner's own, not the helper's.
        let three = vec![running(lead, 0), running(lead, 4), running(helper, 9)];
        assert_eq!(pick_terminal(lead, three, true), TerminalPick::Start(5));
        assert_eq!(
            pick_terminal(helper, vec![running(lead, 3)], true),
            TerminalPick::Start(0)
        );
    }

    /// A transcript of a whole workset steers one of its agents only when
    /// there is one; its tools are the workset's, by its lead.
    #[test]
    fn a_workset_transcript_steers_its_one_agent_or_asks() {
        let (lead, helper) = (agent(1), agent(2));
        let workset = "0123456789ab".parse().unwrap();
        let here = |scope, agents| {
            read_here(
                Some((scope, agents)),
                None,
                &Subject::Own,
                None,
                Vec::new(),
                |agent| agent,
            )
        };
        let alone = here(Scope::Workset(workset), vec![lead]);
        assert_eq!(alone.steer, Target::One(lead));
        assert_eq!(alone.work, Target::One(lead));
        let shared = here(Scope::Workset(workset), vec![lead, helper]);
        assert_eq!(shared.steer, Target::Among(vec![lead, helper]));
        assert_eq!(
            shared.work,
            Target::One(lead),
            "one workset, however many agents"
        );
        let narrowed = here(Scope::Agent(helper), vec![helper]);
        assert_eq!(narrowed.steer, Target::One(helper));
        assert_eq!(narrowed.work, Target::One(helper));
    }

    /// In a thread, the message under the point names its writer; a draft
    /// names its worker; otherwise the thread's members, asked among when
    /// there are several. Elsewhere nothing is in view and commands ask.
    #[test]
    fn a_thread_names_the_agent_from_the_point_outwards() {
        let (lead, helper, other) = (agent(1), agent(2), agent(3));
        let leads = move |agent| if agent == helper { lead } else { agent };
        let thread = |subject: Subject, written, members| {
            read_here(None, None, &subject, written, members, leads)
        };
        let point_on_helper = thread(
            Subject::Own,
            Some(written(helper, true)),
            vec![lead, helper],
        );
        assert_eq!(point_on_helper.steer, Target::One(helper));
        assert_eq!(
            point_on_helper.work,
            Target::One(lead),
            "the helper's workset is its lead's"
        );

        let draft = thread(Subject::Draft { worker: None }, None, vec![lead, other]);
        assert_eq!(draft.steer, Target::Among(vec![lead, other]));
        assert_eq!(draft.work, Target::Among(vec![lead, other]));

        let shared_workset = thread(Subject::None, None, vec![lead, helper]);
        assert_eq!(shared_workset.steer, Target::Among(vec![lead, helper]));
        assert_eq!(
            shared_workset.work,
            Target::One(lead),
            "two agents of one workset leave no doubt which workset"
        );

        let worker = thread(
            Subject::Draft {
                worker: Some(other),
            },
            Some(written(helper, false)),
            vec![lead, other],
        );
        assert_eq!(worker.steer, Target::One(other));

        // The desk names a card's newest writer for `look into the work`
        // without pinning a steering target on it.
        let desk = thread(
            Subject::None,
            Some(written(helper, false)),
            vec![lead, other],
        );
        assert_eq!(desk.steer, Target::Among(vec![lead, other]));

        let nowhere = read_here(None, None, &Subject::None, None, Vec::new(), leads);
        assert_eq!(nowhere.steer, Target::Anything);
        assert_eq!(nowhere.work, Target::Anything);

        let tool = read_here(None, Some(helper), &Subject::Own, None, Vec::new(), leads);
        assert_eq!(tool.steer, Target::One(helper));
        assert_eq!(
            tool.work,
            Target::One(helper),
            "a tool screen's new tools start beside it"
        );
    }
}
