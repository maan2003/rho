//! A push or a fetch that waits for the user's word, in the thread of the
//! work that asks (`product/agents.md`, `flow Approve a push or a fetch`).
//!
//! When an agent pushes to `main` or `master`, or reaches a repository
//! through the user's ssh login, its host asks every window that holds the
//! login and waits for as long as it takes. The request shows at the top of
//! the thread the agent works in, right below the draft, and that thread
//! waits as `approve push · 4m`. `space a` approves; `space x` denies and
//! sends what the draft holds to the agent. Neither is a verdict the window
//! keeps, so `space u` takes neither back, and a reply in the thread
//! answers neither.
//!
//! The requests are the host's: this window keeps only what each host has
//! said waits, and forgets a host's requests when its stream goes, as the
//! host asks again when it is back. A request shows once the window knows
//! the thread its agent works in; one from outside every agent has no
//! thread to show in, and is refused at once.

use std::collections::BTreeMap;

use gpui::{App, Context};
use jiff::Timestamp;
use rho_agent_hosts::HostId;
use rho_agent_hosts::git::{
    GitAsk, GitChange, GitEvent, GitStreams, display_field, display_lines, ref_name,
};
use rho_agent_hosts::protocol::{GitDecision, GitPush, GitRefChange, MAX_PUSH_DIFF_BYTES};
use rho_dealer::curve::{AGENT_BLOCKED_HEAD_START, Curve};
use rho_dealer::facts::Seen;
use rho_dealer::rank::CardKind;
use rho_dealer::{Addressed as _, Claim};
use rho_frame::Row;
use rho_window::style::StyleClass;

use super::{ChatSystem, Event};
use crate::{ThreadId, cards};

/// What the user can say to a request, on the line below it.
pub(crate) const HINT: &str =
    "space a approves · space x denies (with draft reply) · space s snoozes";

/// Every request that waits for the user, by the id its host gave it.
/// Hosts draw ids at random, so one id names one request on every host and
/// across restarts.
pub(super) struct GitAsks {
    pub(super) streams: GitStreams,
    waiting: BTreeMap<u64, Waiting>,
}

/// A request that waits, and the host that asked.
pub(crate) struct Waiting {
    pub host: HostId,
    pub ask: GitAsk,
}

/// What a host said, as the window takes it.
pub(super) enum Heard {
    /// What waits changed.
    Changed,
    /// Nothing waits differently; the echo line says this.
    Echo(String, StyleClass),
}

impl GitAsks {
    pub(super) fn new(streams: GitStreams) -> Self {
        Self {
            streams,
            waiting: BTreeMap::new(),
        }
    }

    /// What a host, which the user calls `name`, said about its requests.
    pub(super) fn heard(&mut self, GitEvent { host, change }: GitEvent, name: &str) -> Heard {
        match change {
            GitChange::Asked(ask) if ask.request.agent.is_none() => {
                let what = ask.what();
                let echo = match ask.answer(GitDecision::Deny) {
                    Ok(()) => format!(
                        "{name}: refused a {what} · it came from outside every agent's work, \
                         so no thread can show it"
                    ),
                    Err(_) => format!("{name}: not connected"),
                };
                Heard::Echo(echo, StyleClass::SystemImportant)
            }
            GitChange::Asked(ask) => {
                self.waiting.insert(ask.request_id, Waiting { host, ask });
                Heard::Changed
            }
            GitChange::Ended { request_id } => {
                if self
                    .waiting
                    .get(&request_id)
                    .is_some_and(|waiting| waiting.host == host)
                {
                    self.waiting.remove(&request_id);
                }
                Heard::Changed
            }
            GitChange::Lost => {
                self.forget_host(host);
                Heard::Changed
            }
            GitChange::Refused { reason, .. } => Heard::Echo(
                format!("{name}: refused an agent's git without asking · {reason}"),
                StyleClass::SystemImportant,
            ),
            GitChange::Failed { what, error } => Heard::Echo(
                format!("{name}: {what} failed · {error}"),
                StyleClass::StatusError,
            ),
        }
    }

    pub(super) fn forget_host(&mut self, host: HostId) {
        self.waiting.retain(|_, waiting| waiting.host != host);
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (u64, &Waiting)> {
        self.waiting.iter().map(|(id, waiting)| (*id, waiting))
    }

    /// The user's word on a request, handed to its host; it no longer
    /// waits here. Says what the echo line reads, given what the user calls
    /// the host.
    pub(super) fn answer(
        &mut self,
        request_id: u64,
        decision: GitDecision,
        host_name: impl FnOnce(HostId) -> String,
    ) -> String {
        let verb = match decision {
            GitDecision::Approve => "approved",
            GitDecision::Deny => "denied",
        };
        let Some(Waiting { host, ask }) = self.waiting.remove(&request_id) else {
            return format!("{verb}: it no longer waits");
        };
        let title = title(&ask);
        match ask.answer(decision) {
            Ok(()) => format!("{verb}: {title}"),
            Err(_) => format!(
                "{}: not connected · it asks again when it is back",
                host_name(host)
            ),
        }
    }
}

/// What a request does, where, in short: `push branch main to
/// github.com/acme/repo`.
pub(crate) fn title(ask: &GitAsk) -> String {
    let repository = format!(
        "{}/{}",
        display_field(&ask.request.host),
        display_field(&ask.request.repository)
    );
    match ask.push() {
        Some(_) => format!("push {} to {repository}", ask.refs().join(", ")),
        None => format!("fetch from {repository}"),
    }
}

impl Waiting {
    /// When the agent's git asked, on the host's clock.
    pub(crate) fn since(&self) -> Timestamp {
        Timestamp::from_millisecond(self.ask.requested_at_ms as i64)
            .unwrap_or_else(|_| Timestamp::now())
    }

    /// What it asks to do, as the card says it: `approve push`.
    pub(crate) fn action(&self) -> &'static str {
        match self.ask.push() {
            Some(_) => "push",
            None => "fetch",
        }
    }

    /// The request as the thread shows it, one fact a line, `asker` being
    /// what the thread calls the agent that asks. The answers come before
    /// the diff, which can be long.
    pub(crate) fn text(&self, asker: &str, asked: &str) -> String {
        text(
            asker,
            &self.ask.destination(),
            self.ask.push(),
            self.action(),
            asked,
        )
    }
}

fn text(
    asker: &str,
    destination: &str,
    push: Option<&GitPush>,
    action: &str,
    asked: &str,
) -> String {
    // The thread's font is proportional, so nothing lines up in columns:
    // each item sits indented on its own line.
    let mut lines = vec![
        format!("{asker} asks to {action} with your ssh login"),
        format!("repository {destination}"),
    ];
    match push {
        Some(push) => push_lines(push, &mut lines),
        None => lines.push(
            "a fetch changes nothing there · Git names the refs it wants once you approve"
                .to_owned(),
        ),
    }
    lines.push(format!("asked {asked}"));
    lines.push(HINT.to_owned());
    if let Some(push) = push {
        diff_lines(push, &mut lines);
    }
    lines.join("\n")
}

/// Each ref it changes, as the agent's repository last saw the remote, and
/// the commits it carries.
fn push_lines(push: &GitPush, lines: &mut Vec<String>) {
    lines.push("changes, from where the agent's repository last saw them".to_owned());
    lines.extend(push.updates.iter().map(|update| {
        let name = ref_name(&update.reference);
        let change = match &update.change {
            GitRefChange::Create { new } => format!("new at {}", short(new)),
            GitRefChange::FastForward { old, new } => format!("{}..{}", short(old), short(new)),
            GitRefChange::Force { old, new, dropped } => {
                let dropped = match dropped {
                    Some(1) => "drops 1 commit".to_owned(),
                    Some(count) => format!("drops {count} commits"),
                    None => "drops commits the agent never fetched".to_owned(),
                };
                format!("FORCE-PUSH {}..{} · {dropped}", short(old), short(new))
            }
            GitRefChange::Delete { old: Some(old) } => format!("deleted, was {}", short(old)),
            GitRefChange::Delete { old: None } => "deleted".to_owned(),
        };
        format!("  {name}  {change}")
    }));
    lines.push(match push.commit_count {
        1 => "1 commit".to_owned(),
        count => format!("{count} commits"),
    });
    lines.extend(push.commits.iter().map(|commit| {
        let date = Timestamp::from_second(commit.authored_at).map_or_else(
            |_| "?".to_owned(),
            |at| {
                at.to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d")
                    .to_string()
            },
        );
        format!(
            "  {} {} · {} · {date}",
            display_field(&commit.short_id),
            display_field(&commit.subject),
            display_field(&commit.author),
        )
    }));
    let unlisted = push.commit_count.saturating_sub(push.commits.len() as u64);
    if unlisted > 0 {
        lines.push(format!("  and {unlisted} more"));
    }
}

/// What each ref's update changes, stat first, then the patch.
fn diff_lines(push: &GitPush, lines: &mut Vec<String>) {
    for diff in &push.diffs {
        lines.push(String::new());
        lines.push(format!("diff of {}", ref_name(&diff.reference)));
        lines.extend(display_lines(&diff.text));
    }
    if push.diff_cut {
        lines.push(String::new());
        lines.push(format!(
            "the diff stops here: it is longer than {} KiB",
            MAX_PUSH_DIFF_BYTES / 1024
        ));
    }
}

/// An object id as the thread shows it.
fn short(object: &str) -> &str {
    &object[..object.len().min(8)]
}

impl ChatSystem {
    pub(super) fn git_heard(&mut self, event: GitEvent, cx: &mut Context<Self>) {
        let name = self
            .hosts
            .get(&event.host)
            .map_or_else(|| event.host.to_string(), |slot| slot.name.clone());
        match self.git.heard(event, &name) {
            Heard::Changed => {
                cx.notify();
                self.frame.changed(cx);
            }
            Heard::Echo(words, class) => self.frame.echo(words, class, cx),
        }
    }

    /// The thread the agent behind a request works in, once this window
    /// holds that thread.
    fn ask_thread(&self, waiting: &Waiting) -> Option<ThreadId> {
        let host = self.hosts.get(&waiting.host)?.endpoint?;
        let agent = waiting.ask.request.agent?;
        let message = self.logs.get(&host)?.working_thread(agent)?;
        Some(ThreadId { host, message })
    }

    /// The requests that wait in `thread`, the oldest first: the one the
    /// thread's answers go to leads.
    pub(crate) fn asks_in(&self, thread: ThreadId) -> Vec<(u64, &Waiting)> {
        let mut asks: Vec<_> = self
            .git
            .iter()
            .filter(|(_, waiting)| self.ask_thread(waiting) == Some(thread))
            .collect();
        asks.sort_by_key(|(id, waiting)| (waiting.ask.requested_at_ms, *id));
        asks
    }

    /// A thread with a request that waits is a card that says so, from
    /// when its oldest request was asked, whatever its mail says. Its mail
    /// still says what a reply in it settles.
    pub(super) fn git_claims(&self, claims: &mut Vec<Claim>, cx: &App) {
        let mut oldest: BTreeMap<ThreadId, (u64, &Waiting)> = BTreeMap::new();
        for (id, waiting) in self.git.iter() {
            let Some(thread) = self.ask_thread(waiting) else {
                continue;
            };
            let entry = oldest.entry(thread).or_insert((id, waiting));
            if (waiting.ask.requested_at_ms, id) < (entry.1.ask.requested_at_ms, entry.0) {
                *entry = (id, waiting);
            }
        }
        for (thread, (id, waiting)) in oldest {
            let address = thread.address();
            let curve = Curve::Waiting {
                head_start: AGENT_BLOCKED_HEAD_START,
                since: waiting.since(),
            };
            let kind = CardKind::Approve(waiting.action());
            let cursor = format!("git/{id:016x}");
            match claims.iter_mut().find(|claim| claim.address == address) {
                Some(claim) => {
                    claim.curve = curve;
                    claim.kind = kind;
                    claim.cursor = format!("{}+{cursor}", claim.cursor);
                }
                None => {
                    let mut claim = Claim::new(address, curve, kind, cursor);
                    if let Some((title, context)) = self.thread_name(thread, cx) {
                        claim.title = title;
                        claim.context = context;
                    }
                    claims.push(claim);
                }
            }
        }
    }

    /// `approve` and `deny` on the oldest request in `thread`, when one
    /// waits.
    pub(super) fn git_verdicts(
        &mut self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> Option<Vec<Row>> {
        let request = self.asks_in(thread).first()?.0;
        let address = thread.address();
        let answer = |key, word, decision| {
            let chat = cx.entity();
            let address = address.clone();
            Row::run(key, word, move |frame, window, cx| {
                let seen = frame.card_seen(&address, cx);
                let said = chat.update(cx, |chat, cx| {
                    chat.answer_git(thread, request, decision, seen, cx)
                });
                frame.answered(&address, said, window, cx);
            })
        };
        Some(vec![
            answer("a", "approve", GitDecision::Approve),
            answer("x", "deny", GitDecision::Deny),
        ])
    }

    /// The user's word on `request` in `thread`, handed to its host. A
    /// denial carries what the thread's draft holds to its agents, as a
    /// reply that settles the thread through `seen`, and the draft empties.
    /// Says what the echo line reads.
    pub(super) fn answer_git(
        &mut self,
        thread: ThreadId,
        request: u64,
        decision: GitDecision,
        seen: Option<Seen>,
        cx: &mut Context<Self>,
    ) -> String {
        let mut words_sent = false;
        if matches!(decision, GitDecision::Deny) {
            let words = self.draft(thread.host, Some(thread.message)).to_owned();
            if !words.trim().is_empty() {
                let settles = seen.and_then(|seen| {
                    cards::answered(self.host_chat(thread.host)?, thread.message, seen)
                });
                self.reply(thread, settles, words, cx);
                self.set_draft(thread.host, Some(thread.message), String::new());
                cx.emit(Event::DraftSent(thread));
                words_sent = true;
            }
        }
        let hosts = &self.hosts;
        let mut said = self.git.answer(request, decision, |host| {
            hosts
                .get(&host)
                .map_or_else(|| host.to_string(), |slot| slot.name.clone())
        });
        if words_sent {
            said.push_str(" · your words went with it");
        }
        cx.notify();
        self.frame.changed(cx);
        said
    }
}

#[cfg(test)]
pub(super) mod tests {
    use gpui::{Entity, TestAppContext, WindowHandle};
    use rho_agent_hosts::git::GitAnswered;
    use rho_agent_hosts::protocol::{
        GitCommit, GitDiff, GitIntent, GitRefUpdate, GitTransportRequest,
    };
    use rho_agent_types::{
        Address as Party, AgentId, AgentIdDomain, ChatEvent, ChatRecord, ChatSeq, ContentPart,
        EndpointId, MessageId, UnixMs, Voice,
    };
    use rho_frame::{Frame, System as _};

    use super::*;
    use crate::HostChat;

    /// A push that creates `refs` and says nothing more.
    pub(crate) fn push_to(refs: &[&str], agent: Option<AgentId>) -> GitTransportRequest {
        request(
            GitIntent::Push(GitPush {
                updates: refs
                    .iter()
                    .map(|reference| GitRefUpdate {
                        reference: (*reference).to_owned(),
                        change: GitRefChange::Create {
                            new: "1".repeat(40),
                        },
                    })
                    .collect(),
                commits: Vec::new(),
                commit_count: 0,
                diffs: Vec::new(),
                diff_cut: false,
            }),
            agent,
        )
    }

    fn request(intent: GitIntent, agent: Option<AgentId>) -> GitTransportRequest {
        GitTransportRequest {
            host: "github.com".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "acme/repo".to_owned(),
            intent,
            agent,
        }
    }

    const HOST: EndpointId = EndpointId([3; 32]);
    const LAB: HostId = HostId(7);
    const PROJECT: MessageId = MessageId(ChatSeq(1));
    /// The thread `worker` took up first, and `standing` too.
    const FLAKY: MessageId = MessageId(ChatSeq(3));

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &AgentIdDomain(0)).unwrap()
    }

    fn worker() -> AgentId {
        agent(1)
    }

    fn posted(seq: u64, parent: MessageId, from: Party, text: &str) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(1_760_000_000_000 + seq),
            event: ChatEvent::Posted {
                parent: Some(parent),
                retry_of: None,
                thread_reply: None,
                from,
                voice: Voice::Message,
                content: vec![ContentPart::Text { text: text.into() }],
            },
        }
    }

    fn joined(seq: u64, thread: MessageId, agent: AgentId) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(1_760_000_000_000 + seq),
            event: ChatEvent::Joined { thread, agent },
        }
    }

    struct World {
        window: WindowHandle<Frame>,
        mail: Entity<ChatSystem>,
        next: u64,
        _db: tempfile::TempDir,
    }

    /// The project `tide` on `lab`, where `worker` works on a flaky test.
    fn world(cx: &mut TestAppContext) -> World {
        let mut log = HostChat::default();
        log.tell(&ChatRecord {
            seq: ChatSeq(1),
            at: UnixMs(1_760_000_000_001),
            event: ChatEvent::Posted {
                parent: None,
                retry_of: None,
                thread_reply: None,
                from: Party::User,
                voice: Voice::Message,
                content: vec![ContentPart::Text {
                    text: "tide".into(),
                }],
            },
        });
        log.tell(&ChatRecord {
            seq: ChatSeq(2),
            at: UnixMs(1_760_000_000_002),
            event: ChatEvent::Project {
                project: rho_agent_types::ProjectId(PROJECT),
                config: rho_agent_types::ProjectConfig {
                    place: rho_agent_types::ProjectPlace::parse("~/src/tide").unwrap(),
                    mode: rho_agent_types::ProjectMode::One,
                    runtime: rho_agent_types::RuntimeLabel::default(),
                },
            },
        });
        log.tell(&posted(3, PROJECT, Party::User, "Flaky clock test"));
        log.tell(&joined(4, FLAKY, worker()));
        let (window, mail, db) = super::super::search_bench::fixture(cx, log);
        cx.update(|cx| {
            mail.update(cx, |chat, cx| {
                chat.streams(
                    LAB,
                    "lab",
                    Some(HOST),
                    &rho_agent_hosts::Link::detached(),
                    cx,
                );
            })
        });
        World {
            window,
            mail,
            next: 5,
            _db: db,
        }
    }

    impl World {
        fn tell(&mut self, cx: &mut TestAppContext, record: impl FnOnce(u64) -> ChatRecord) {
            let record = record(self.next);
            self.next += 1;
            self.mail.update(cx, |chat, cx| {
                chat.logs.get_mut(&HOST).unwrap().tell(&record);
                chat.changed(HOST, cx);
            });
            cx.run_until_parked();
        }

        /// The host says `request` waits, asked `ago` minutes back; the
        /// answer arrives at what this returns.
        fn asks(
            &self,
            cx: &mut TestAppContext,
            id: u64,
            request: GitTransportRequest,
            ago: i64,
        ) -> GitAnswered {
            let at = Timestamp::now() - jiff::SignedDuration::from_mins(ago);
            let (ask, answered) = GitAsk::for_test(id, request, at.as_millisecond() as u64);
            self.hears(cx, GitChange::Asked(ask));
            answered
        }

        fn hears(&self, cx: &mut TestAppContext, change: GitChange) {
            self.mail.update(cx, |chat, cx| {
                chat.git_heard(GitEvent { host: LAB, change }, cx)
            });
            cx.run_until_parked();
        }

        /// The hand as `heading · label`.
        fn hand(&self, cx: &mut TestAppContext) -> Vec<String> {
            cx.run_until_parked();
            self.window
                .update(cx, |frame, _, _| {
                    frame
                        .hand()
                        .cards
                        .iter()
                        .map(|card| format!("{} · {}", card.heading(), card.label))
                        .collect()
                })
                .unwrap()
        }

        fn current(&self, cx: &mut TestAppContext) -> rho_dealer::Address {
            self.window
                .update(cx, |frame, _, _| frame.current_address().clone())
                .unwrap()
        }

        /// The text of the screen in view.
        fn screen(&self, cx: &mut TestAppContext) -> String {
            cx.run_until_parked();
            self.window
                .update(cx, |frame, _, cx| {
                    frame
                        .current_surface()
                        .editor(cx)
                        .unwrap()
                        .read(cx)
                        .text(cx)
                })
                .unwrap()
        }

        fn deal(&self, cx: &mut TestAppContext) {
            self.window
                .update(cx, |frame, window, cx| frame.pull_card(window, cx))
                .unwrap();
            cx.run_until_parked();
        }

        /// `space`: the leader's rows over the screen in view.
        fn leader(&self, cx: &mut TestAppContext) -> Vec<(String, String)> {
            cx.dispatch_action(self.window.into(), rho_frame::keys::Leader);
            cx.run_until_parked();
            self.window
                .update(cx, |frame, _, _| frame.menu_rows())
                .unwrap()
        }

        /// `space` then `key`.
        fn press(&self, cx: &mut TestAppContext, key: &str) {
            assert!(!self.leader(cx).is_empty(), "the leader is open");
            cx.simulate_keystrokes(self.window.into(), key);
            cx.run_until_parked();
        }

        fn set_draft(&self, cx: &mut TestAppContext, thread: MessageId, text: &str) {
            self.mail.update(cx, |chat, _| {
                chat.set_draft(HOST, Some(thread), text.into())
            });
        }

        fn draft(&self, cx: &mut TestAppContext, thread: MessageId) -> String {
            self.mail
                .read_with(cx, |chat, _| chat.draft(HOST, Some(thread)).to_owned())
        }

        /// The words this device queued for the host under `thread`.
        fn sent(&self, cx: &mut TestAppContext, thread: MessageId) -> Vec<String> {
            self.mail.read_with(cx, |chat, _| {
                chat.local_under(HOST, Some(thread))
                    .iter()
                    .map(|outgoing| rho_agent_types::content_text(&outgoing.content))
                    .collect()
            })
        }
    }

    fn flaky() -> rho_dealer::Address {
        ThreadId {
            host: HOST,
            message: FLAKY,
        }
        .address()
    }

    /// An agent's push to `main` waits in the thread the agent works in:
    /// the thread is the card, `approve push · 5m`, and nothing pops up.
    /// Dealt, the thread shows what the push would do right below its
    /// draft, and the leader offers approve, deny and snooze, never done
    /// or mute; `space d` decides nothing.
    #[gpui::test]
    fn a_push_waits_in_the_thread_of_its_work(cx: &mut TestAppContext) {
        let world = world(cx);
        let mut answered = world.asks(cx, 0xa1, push_to(&["refs/heads/main"], Some(worker())), 5);
        assert_eq!(
            world.hand(cx),
            ["tide › Flaky clock test · approve push · 5m"]
        );
        assert_eq!(
            world.current(cx),
            Frame::desk_address(),
            "nothing popped up"
        );

        world.deal(cx);
        assert_eq!(world.current(cx), flaky());
        let screen = world.screen(cx);
        let lines: Vec<&str> = screen.lines().collect();
        let label = worker().encoded();
        assert_eq!(
            lines[..7],
            [
                "",
                "",
                &format!("{label} asks to push with your ssh login"),
                "repository ssh://github.com:22/acme/repo",
                "changes, from where the agent's repository last saw them",
                "  branch main  new at 11111111",
                "0 commits",
            ]
        );
        assert!(lines[7].starts_with("asked "));
        assert_eq!(lines[8], HINT);
        assert_eq!(
            lines[10], "Flaky clock test",
            "the thread's messages follow"
        );

        let keys = |rows: Vec<(String, String)>| {
            rows.into_iter()
                .map(|(key, word)| format!("{key} {word}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(world.leader(cx)),
            ["f places", "a approve", "x deny", "s snooze"]
        );
        cx.simulate_keystrokes(world.window.into(), "d");
        cx.run_until_parked();
        assert!(answered.try_recv().is_err(), "space d decides nothing");
        assert_eq!(world.hand(cx).len(), 1);
    }

    /// `space x` refuses the push and sends what the draft holds to the
    /// thread's agents as a reply, empties the draft and deals on; with an
    /// empty draft it only refuses. Neither leaves anything to undo.
    #[gpui::test]
    fn deny_refuses_with_the_drafts_words_and_deals_on(cx: &mut TestAppContext) {
        let mut world = world(cx);
        world.tell(cx, |seq| posted(seq, PROJECT, Party::User, "Second idea"));
        let second = MessageId(ChatSeq(world.next - 1));
        world.tell(cx, |seq| joined(seq, second, agent(2)));
        let mut first = world.asks(cx, 0xa1, push_to(&["refs/heads/main"], Some(worker())), 5);
        let mut other = world.asks(cx, 0xb2, push_to(&["refs/tags/v1"], Some(agent(2))), 1);
        let words = "Push a branch and open a PR instead.";
        world.set_draft(cx, FLAKY, words);

        world.deal(cx);
        assert_eq!(world.current(cx), flaky());
        world.press(cx, "x");
        assert_eq!(first.try_recv(), Ok(GitDecision::Deny));
        assert_eq!(world.sent(cx, FLAKY), [words]);
        assert_eq!(world.draft(cx, FLAKY), "");
        assert!(!world.screen(cx).starts_with(words), "the draft is empty");
        let second_thread = ThreadId {
            host: HOST,
            message: second,
        }
        .address();
        assert_eq!(world.current(cx), second_thread, "the next card is dealt");

        world.press(cx, "x");
        assert_eq!(other.try_recv(), Ok(GitDecision::Deny));
        assert_eq!(world.sent(cx, second), Vec::<String>::new());
        assert!(world.hand(cx).is_empty());
        assert_eq!(world.current(cx), Frame::desk_address());
        assert!(
            !world.leader(cx).iter().any(|(key, _)| key == "u"),
            "nothing to undo"
        );
    }

    /// `space a` hands the approval to the host and deals on; the draft
    /// stays for later.
    #[gpui::test]
    fn approve_answers_the_host_and_keeps_the_draft(cx: &mut TestAppContext) {
        let world = world(cx);
        let mut main = world.asks(cx, 0xa1, push_to(&["refs/heads/main"], Some(worker())), 5);
        world.set_draft(cx, FLAKY, "Half an answer");
        world.deal(cx);
        world.press(cx, "a");
        assert_eq!(main.try_recv(), Ok(GitDecision::Approve));
        assert_eq!(world.draft(cx, FLAKY), "Half an answer");
        assert_eq!(world.sent(cx, FLAKY), Vec::<String>::new());
        assert!(world.hand(cx).is_empty());
        assert_eq!(world.current(cx), Frame::desk_address());
    }

    /// A reply in a thread whose push waits settles the message it
    /// answers and leaves the push waiting; answering the push then puts
    /// the card away.
    #[gpui::test]
    fn a_reply_leaves_the_push_waiting(cx: &mut TestAppContext) {
        let mut world = world(cx);
        world.tell(cx, |seq| {
            posted(seq, FLAKY, Party::Agent(worker()), "Shall I push it?")
        });
        assert_eq!(world.hand(cx).len(), 1);
        let mut main = world.asks(cx, 0xa1, push_to(&["refs/heads/main"], Some(worker())), 5);
        assert_eq!(
            world.hand(cx),
            ["tide › Flaky clock test · approve push · 5m"]
        );
        world.set_draft(cx, FLAKY, "Yes, once it is green.");
        world.deal(cx);
        cx.dispatch_action(world.window.into(), rho_frame::thread::SendMessage);
        cx.run_until_parked();
        assert_eq!(world.sent(cx, FLAKY), ["Yes, once it is green."]);
        assert!(main.try_recv().is_err(), "the reply decides nothing");
        assert_eq!(
            world.hand(cx),
            ["tide › Flaky clock test · approve push · 5m"],
            "the push still waits"
        );
        let said = world.mail.update(cx, |chat, cx| {
            chat.answer_git(
                ThreadId {
                    host: HOST,
                    message: FLAKY,
                },
                0xa1,
                GitDecision::Approve,
                None,
                cx,
            )
        });
        assert_eq!(said, "approved: push branch main to github.com/acme/repo");
        assert_eq!(
            world.hand(cx),
            Vec::<String>::new(),
            "the reply settled the message"
        );
    }

    /// A request shows once the window knows the thread its agent works
    /// in, and an agent that took up another idea asks in that one. A
    /// request that ends takes its card and its lines with it; a host whose
    /// stream goes takes every request it asked.
    #[gpui::test]
    fn a_request_shows_in_the_thread_its_agent_works_in_until_it_ends(cx: &mut TestAppContext) {
        let mut world = world(cx);
        let _early = world.asks(cx, 0xa1, push_to(&["refs/heads/main"], Some(agent(5))), 3);
        assert!(world.hand(cx).is_empty(), "no thread shows it yet");
        world.tell(cx, |seq| joined(seq, FLAKY, agent(5)));
        assert_eq!(
            world.hand(cx),
            ["tide › Flaky clock test · approve push · 3m"]
        );
        // The standing agent took up the next idea: its next push asks
        // there.
        world.tell(cx, |seq| posted(seq, PROJECT, Party::User, "Next idea"));
        let next = MessageId(ChatSeq(world.next - 1));
        world.tell(cx, |seq| joined(seq, next, worker()));
        let _later = world.asks(cx, 0xb2, request(GitIntent::Fetch, Some(worker())), 1);
        assert_eq!(
            world.hand(cx),
            [
                "tide › Flaky clock test · approve push · 3m",
                "tide › Next idea · approve fetch · 1m",
            ]
        );
        world.deal(cx);
        assert!(world.screen(cx).contains("asks to push"));
        world.hears(cx, GitChange::Ended { request_id: 0xa1 });
        assert!(!world.screen(cx).contains("asks to push"), "its lines went");
        assert_eq!(world.hand(cx), ["tide › Next idea · approve fetch · 1m"]);
        world.hears(cx, GitChange::Lost);
        assert!(world.hand(cx).is_empty());
    }

    /// A request from outside every agent's work has no thread to show in:
    /// the window refuses it at once and says so.
    #[gpui::test]
    fn a_request_from_outside_every_agent_is_refused(cx: &mut TestAppContext) {
        let world = world(cx);
        let mut answered = world.asks(cx, 0xa1, push_to(&["refs/heads/main"], None), 1);
        assert_eq!(answered.try_recv(), Ok(GitDecision::Deny));
        assert!(world.hand(cx).is_empty());
    }

    fn date(seconds: i64) -> String {
        Timestamp::from_second(seconds)
            .unwrap()
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d")
            .to_string()
    }

    /// A push says who asks, each ref it changes with a force-push called
    /// out with what it drops, the commits it carries, and its diff, and
    /// says where that stops; the answers come before the diff. What the
    /// agent's repository wrote shows no control characters.
    #[test]
    fn a_push_shows_its_refs_commits_and_diff() {
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let commit = |short_id: &str, subject: &str, authored_at| GitCommit {
            short_id: short_id.to_owned(),
            author: "Ada Lovelace".to_owned(),
            authored_at,
            subject: subject.to_owned(),
        };
        let update = |reference: &str, change| GitRefUpdate {
            reference: reference.to_owned(),
            change,
        };
        let push = GitPush {
            updates: vec![
                update(
                    "refs/heads/main",
                    GitRefChange::FastForward {
                        old: a.clone(),
                        new: b.clone(),
                    },
                ),
                update(
                    "refs/heads/rewrite",
                    GitRefChange::Force {
                        old: c.clone(),
                        new: b.clone(),
                        dropped: Some(2),
                    },
                ),
                update(
                    "refs/heads/stale",
                    GitRefChange::Force {
                        old: c.clone(),
                        new: b.clone(),
                        dropped: None,
                    },
                ),
                update("refs/tags/v1", GitRefChange::Create { new: b.clone() }),
                update("refs/heads/gone", GitRefChange::Delete { old: Some(a) }),
            ],
            commits: vec![
                commit("bbbbbbb", "Test the clock", 1_764_000_180),
                commit("ddddddd", "Fix the \u{202e}clock", 1_700_000_000),
            ],
            commit_count: 7,
            diffs: vec![GitDiff {
                reference: "refs/heads/main".to_owned(),
                text: " clock.rs | 2 +-\n\n--- a/clock.rs\n+++ b/clock.rs\n-tick\n+\ttock\n"
                    .to_owned(),
            }],
            diff_cut: true,
        };
        let text = text(
            "eng-q9tz",
            "ssh://github.com:22/acme/repo",
            Some(&push),
            "push",
            "09:12",
        );
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "eng-q9tz asks to push with your ssh login".to_owned(),
                "repository ssh://github.com:22/acme/repo".to_owned(),
                "changes, from where the agent's repository last saw them".to_owned(),
                "  branch main  aaaaaaaa..bbbbbbbb".to_owned(),
                "  branch rewrite  FORCE-PUSH cccccccc..bbbbbbbb · drops 2 commits".to_owned(),
                "  branch stale  FORCE-PUSH cccccccc..bbbbbbbb · drops commits the agent \
                 never fetched"
                    .to_owned(),
                "  tag v1  new at bbbbbbbb".to_owned(),
                "  branch gone  deleted, was aaaaaaaa".to_owned(),
                "7 commits".to_owned(),
                format!(
                    "  bbbbbbb Test the clock · Ada Lovelace · {}",
                    date(1_764_000_180)
                ),
                format!(
                    "  ddddddd Fix the \u{fffd}clock · Ada Lovelace · {}",
                    date(1_700_000_000)
                ),
                "  and 5 more".to_owned(),
                "asked 09:12".to_owned(),
                HINT.to_owned(),
                String::new(),
                "diff of branch main".to_owned(),
                " clock.rs | 2 +-".to_owned(),
                String::new(),
                "--- a/clock.rs".to_owned(),
                "+++ b/clock.rs".to_owned(),
                "-tick".to_owned(),
                "+    tock".to_owned(),
                String::new(),
                "the diff stops here: it is longer than 64 KiB".to_owned(),
            ]
        );
    }

    /// A fetch names the repository, and says that Git names the refs
    /// only after the approval.
    #[test]
    fn a_fetch_says_what_it_can() {
        let text = text(
            "eng-q9tz",
            "ssh://git.sr.ht:22/~alice/repo",
            None,
            "fetch",
            "09:12",
        );
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "eng-q9tz asks to fetch with your ssh login",
                "repository ssh://git.sr.ht:22/~alice/repo",
                "a fetch changes nothing there · Git names the refs it wants once you approve",
                "asked 09:12",
                HINT,
            ]
        );
    }
}
