//! Mail state that is deliberately not in the chat log.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rho_agent_types::{
    Activity, Address, AgentId, Doing, Message, MessageId, Stop, ThreadStatus, UnixMs,
};
use tokio::sync::watch;

use super::AgentPool;
use crate::record::{Event, Record};
use crate::store::{MailRead as _, MailWrite as _, StoreRead as _, StoreWrite as _};
use crate::{InferenceState, RuntimeState};

const COLD_AFTER: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
pub(super) struct Mail {
    statuses: Mutex<BTreeMap<MessageId, Status>>,
    status_changed: watch::Sender<()>,
    presence: Mutex<Presence>,
    retry_checks: Mutex<HashMap<AgentId, tokio::task::JoinHandle<()>>>,
}

struct Status {
    owner: AgentId,
    at: UnixMs,
    text: String,
}

impl Mail {
    fn set_status(
        &self,
        thread: MessageId,
        owner: AgentId,
        text: String,
        at: UnixMs,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !text.contains(['\r', '\n', '\u{85}', '\u{2028}', '\u{2029}']),
            "status is one line"
        );
        let text = text.trim().to_owned();
        anyhow::ensure!(
            text.chars().count() <= 240,
            "status can have at most 240 characters"
        );
        let mut held = self.statuses.lock().expect("poison");
        if text.is_empty() {
            if held.remove(&thread).is_none() {
                return Ok(());
            }
        } else {
            held.insert(thread, Status { owner, at, text });
        }
        self.status_changed.send_replace(());
        Ok(())
    }

    fn clear_stopped_status(&self, agent: AgentId) {
        let mut held = self.statuses.lock().expect("poison");
        let before = held.len();
        held.retain(|_, status| status.owner != agent);
        if held.len() != before {
            self.status_changed.send_replace(());
        }
    }

    fn status_snapshot(&self) -> Vec<ThreadStatus> {
        self.statuses
            .lock()
            .expect("poison")
            .iter()
            .map(|(&thread, status)| ThreadStatus {
                thread,
                at: status.at,
                text: status.text.clone(),
            })
            .collect()
    }
}

#[derive(Default)]
struct Presence(HashMap<MessageId, Visit>);

struct Visit {
    last: UnixMs,
    open: usize,
    warned: bool,
}

impl Presence {
    fn warm(&mut self, thread: MessageId, now: UnixMs) -> &mut Visit {
        let visit = self.0.entry(thread).or_insert_with(|| Visit {
            last: now,
            open: 0,
            warned: false,
        });
        if now > visit.last {
            visit.last = now;
            visit.warned = false;
        }
        visit
    }

    fn cold(&mut self, thread: MessageId, now: UnixMs, last: UnixMs) -> Option<u64> {
        let visit = self.0.entry(thread).or_insert_with(|| Visit {
            last,
            open: 0,
            warned: false,
        });
        let age = now.0.saturating_sub(visit.last.0);
        if visit.open != 0 || age <= COLD_AFTER.as_millis() as u64 || visit.warned {
            return None;
        }
        visit.warned = true;
        Some(age / 60_000)
    }
}

/// The thread in view on a single session. Closing the stream (or the
/// connection) counts as leaving it, even when no final frame arrived.
pub struct MailView {
    pool: Arc<AgentPool>,
    current: Option<MessageId>,
}

impl MailView {
    pub fn set(&mut self, thread: Option<MessageId>) {
        let thread = thread.filter(|id| self.pool.db.read().message(*id).is_some());
        if self.current == thread {
            return;
        }
        let now = UnixMs::now();
        let mut presence = self.pool.mail.presence.lock().expect("poison");
        if let Some(old) = self.current {
            presence.warm(old, now).open -= 1;
        }
        if let Some(new) = thread {
            presence.warm(new, now).open += 1;
        }
        self.current = thread;
    }
}

impl Drop for MailView {
    fn drop(&mut self) {
        self.set(None);
    }
}

impl AgentPool {
    pub fn mail_view(self: &Arc<Self>) -> MailView {
        MailView {
            pool: self.clone(),
            current: None,
        }
    }

    pub fn watch_thread_statuses(&self) -> watch::Receiver<()> {
        self.mail.status_changed.subscribe()
    }

    pub fn thread_statuses(&self) -> Vec<ThreadStatus> {
        let at = UnixMs::now();
        let working: Vec<_> = self
            .doing
            .lock()
            .expect("poison")
            .iter()
            .filter(|(_, doing)| is_working(**doing, at))
            .map(|(&agent, doing)| (agent, doing.since))
            .collect();
        let read = self.db.read();
        let working = working.into_iter().filter_map(|(agent, since)| {
            let head = read.agent(agent)?;
            let thread = read
                .message(MessageId(head.delivered))
                .or_else(|| origin(&read, agent))
                .map(|mail| read.mail_thread(&mail))
                .or_else(|| read.first_thread(agent))?;
            Some((thread, since))
        });
        with_working(self.mail.status_snapshot(), working)
    }

    pub(super) fn activity_changed(&self, agent: AgentId, before: Option<Doing>, now: Doing) {
        let at = UnixMs::now();
        if before.is_some_and(|before| is_working(before, at)) && !is_working(now, at) {
            self.mail.clear_stopped_status(agent);
        }
        self.thread_work_changed();
    }

    pub(super) fn agents_left_mail(&self, agents: &[AgentId]) {
        for &agent in agents {
            self.mail.clear_stopped_status(agent);
        }
    }

    pub(crate) fn thread_work_changed(&self) {
        self.mail.status_changed.send_replace(());
    }

    pub(super) fn set_thread_status(
        &self,
        thread: MessageId,
        agent: AgentId,
        text: String,
    ) -> anyhow::Result<()> {
        let head = self
            .db
            .read()
            .thread(thread)
            .ok_or_else(|| anyhow::anyhow!("no thread #{}", thread.0.0))?;
        anyhow::ensure!(
            head.members.contains(&agent),
            "you are not in thread #{}",
            thread.0.0
        );
        self.mail.set_status(thread, agent, text, UnixMs::now())
    }

    /// Use the project the sender works in; children and advisors inherit
    /// their spawner's project. A reply to direct mail stays with that
    /// conversation, including when the two agents work in different places.
    pub(super) fn mail_project(&self, mut agent: AgentId) -> Option<MessageId> {
        let read = self.db.read();
        let mut seen = std::collections::HashSet::new();
        while seen.insert(agent) {
            if let Some(project) = origin(&read, agent)
                .map(|mail| mail.id)
                .or_else(|| read.first_thread(agent))
                .and_then(|id| read.root_of(id))
            {
                return Some(project);
            }
            let head = read.agent(agent)?;
            let parent = head
                .spec
                .props
                .get("parent")
                .or_else(|| head.spec.props.get("started_by"))?;
            agent = rho_agent_types::parse_full_handle(parent)?;
        }
        None
    }

    pub(super) fn user_visited(&self, message: MessageId) {
        let read = self.db.read();
        if let Some(mail) = read.message(message) {
            let thread = read.mail_thread(&mail);
            self.mail
                .presence
                .lock()
                .expect("poison")
                .warm(thread, mail.at);
        }
    }

    /// Run as an existing model request is prepared; never wake an idle or
    /// stopped agent solely to tell it the thread is cold.
    pub(crate) fn cold_notice(
        &self,
        agent: AgentId,
        incoming: Option<MessageId>,
    ) -> Option<(MessageId, u64)> {
        let read = self.db.read();
        let thread = incoming
            .and_then(|id| read.message(id))
            .or_else(|| origin(&read, agent))
            .map(|mail| read.mail_thread(&mail))
            .or_else(|| read.first_thread(agent))?;
        let anchor = read.message(thread)?;
        let now = UnixMs::now();
        let mut presence = self.mail.presence.lock().expect("poison");
        let last = if presence.0.contains_key(&thread) {
            // Only used to initialize an unknown thread.
            anchor.at
        } else {
            read.last_user_mail(thread).unwrap_or(anchor.at)
        };
        presence
            .cold(thread, now, last)
            .map(|minutes| (thread, minutes))
    }

    pub(super) fn observe_stop(
        self: &Arc<Self>,
        agent: AgentId,
        doing: Doing,
        runtime: &RuntimeState,
    ) {
        // Retry deadlines use the existing Doing clock, not a second
        // definition of stuck. There is only one pending check per agent.
        let mut checks = self.mail.retry_checks.lock().expect("poison");
        if let Some(check) = checks.remove(&agent) {
            check.abort();
        }
        let now = UnixMs::now();
        let reason = match &runtime.inference {
            InferenceState::Failed { error, .. }
            | InferenceState::Lost { error, .. }
            | InferenceState::Retrying { error, .. } => error.clone(),
            _ => return,
        };
        let weak = Arc::downgrade(self);
        if let Some(stop) = doing.stop(now) {
            self.mail.clear_stopped_status(agent);
            tokio::spawn(send_stop(weak, agent, doing, stop, reason));
        } else if let Some(deadline) = doing.stuck_at() {
            let check = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(deadline.0.saturating_sub(now.0))).await;
                let Some(pool) = weak.upgrade() else { return };
                {
                    let mut checks = pool.mail.retry_checks.lock().expect("poison");
                    let current = pool.doing.lock().expect("poison");
                    if current.get(&agent) != Some(&doing) {
                        return;
                    }
                    pool.mail.clear_stopped_status(agent);
                    pool.mail.status_changed.send_replace(());
                    // Leave the write alone if the next status comes just
                    // after this deadline; never remove a newer timer.
                    checks.remove(&agent);
                }
                let latest = pool.get(agent).await.and_then(|client| {
                    match client.status().runtime.inference {
                        InferenceState::Retrying { since, error, .. } if since == doing.since => {
                            Some(error)
                        }
                        _ => None,
                    }
                });
                send_stop(
                    Arc::downgrade(&pool),
                    agent,
                    doing,
                    Stop::Stuck,
                    latest.unwrap_or(reason),
                )
                .await;
            });
            checks.insert(agent, check);
        }
    }
}

fn is_working(doing: Doing, now: UnixMs) -> bool {
    match doing.activity {
        Activity::Working => true,
        Activity::Retrying => doing.stop(now).is_none(),
        _ => false,
    }
}

fn with_working(
    explicit: Vec<ThreadStatus>,
    working: impl IntoIterator<Item = (MessageId, UnixMs)>,
) -> Vec<ThreadStatus> {
    let mut fallback = BTreeMap::<MessageId, UnixMs>::new();
    for (thread, since) in working {
        // The newest worker starting work is the last change to this row.
        fallback
            .entry(thread)
            .and_modify(|at| *at = (*at).max(since))
            .or_insert(since);
    }
    for status in &explicit {
        fallback.remove(&status.thread);
    }
    let mut statuses = explicit;
    statuses.extend(fallback.into_iter().map(|(thread, at)| ThreadStatus {
        thread,
        at,
        text: Activity::Working.word().to_owned(),
    }));
    statuses
}

fn origin(read: &rho_db::ReadTxn, agent: AgentId) -> Option<Message> {
    let head = read.agent(agent)?;
    let origin = head
        .spec
        .props
        .get("origin")?
        .strip_prefix("chat:")?
        .parse::<u64>()
        .ok()?;
    read.message(MessageId(rho_agent_types::ChatSeq(origin)))
}

async fn send_stop(
    weak: Weak<AgentPool>,
    agent: AgentId,
    doing: Doing,
    stop: Stop,
    reason: String,
) {
    let Some(pool) = weak.upgrade() else { return };
    let (workset, thread, body) = {
        let read = pool.db.read();
        let Some(head) = read.agent(agent) else {
            return;
        };
        let thread = read
            .first_thread(agent)
            .or_else(|| origin(&read, agent).map(|mail| read.mail_thread(&mail)));
        let Some(thread) = thread else { return };
        let failed_to_start = head
            .startup_failure
            .is_some_and(|failure| failure.since == doing.since);
        let body = stop_body(&read, agent, stop, &reason, failed_to_start);
        (head.workset, thread, body)
    };
    let mut write = pool.db.write().await;
    let sender = Address::System {
        agent,
        stop,
        since: doing.since,
    };
    let at = UnixMs::now();
    let result = write
        .post_stop(at, thread, sender, body)
        .and_then(|message| {
            if let Some(message) = message {
                write.append(
                    workset,
                    &Record {
                        at,
                        event: Event::MailWritten { agent, message },
                    },
                )?;
            }
            Ok(message)
        });
    match result {
        Ok(Some(_)) => write.commit(),
        Ok(None) => {}
        Err(error) => eprintln!("rho-agent: post stop of {}: {error:#}", agent.encoded()),
    }
}

fn stop_body(
    read: &rho_db::ReadTxn,
    agent: AgentId,
    stop: Stop,
    reason: &str,
    failed_to_start: bool,
) -> String {
    let mut tail = StopTail::default();
    for (_, row) in read.agent_log_before(agent, rho_agent_types::Wpos(u64::MAX), 32) {
        let fragments = match row.event {
            Event::Request { output, inputs, .. } => inputs
                .iter()
                .map(|input| rho_agent_types::content_text(&input.content))
                .chain(std::iter::once(rho_agent_types::content_text(&output)))
                .collect(),
            Event::Step { prose, exec, .. } => [Some(prose), exec].into_iter().flatten().collect(),
            Event::ClaudeReply { text, .. } => vec![text],
            Event::Failed { partial, error, .. } => vec![partial, error.into_owned()],
            Event::Notice { notice, .. } => match notice {
                crate::entry::Notice::Error(error) | crate::entry::Notice::Stopped(error) => {
                    vec![error]
                }
                crate::entry::Notice::StartupFailed(failure) => vec![failure.error],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        for line in fragments
            .iter()
            .rev()
            .flat_map(|fragment| fragment.lines().rev())
        {
            if !tail.push(line) {
                return tail.body(stop, reason, failed_to_start);
            }
        }
    }
    tail.body(stop, reason, failed_to_start)
}

/// Only the stop mail is abbreviated; the transcript keeps the exact errors.
#[derive(Default)]
struct StopTail(Vec<(String, usize)>);

impl StopTail {
    /// Reads newest to oldest. Count a whole run even at the five-line limit.
    fn push(&mut self, line: &str) -> bool {
        const MAX_LINES: usize = 5;
        let line = line.trim();
        if line.is_empty() {
            return true;
        }
        if let Some((last, count)) = self.0.last_mut()
            && last == line
        {
            *count += 1;
        } else if self.0.len() < MAX_LINES {
            self.0.push((line.to_owned(), 1));
        } else {
            return false;
        }
        true
    }

    fn body(self, stop: Stop, reason: &str, failed_to_start: bool) -> String {
        let reason = rho_agent_types::one_line(reason);
        let mut occurrences = 0;
        let mut evidence = Vec::new();
        for (line, count) in self.0.into_iter().rev() {
            if rho_agent_types::one_line(&line) == reason {
                occurrences += count;
            } else {
                evidence.push(stop_line(&line, count));
            }
        }
        let reason = stop_line(&reason, occurrences);
        let mut body = format!("{} · {reason}", stop.word());
        if failed_to_start {
            body.push_str("\n\nThis agent cannot start. Replies are saved. Change its runtime or start another agent.");
        }
        if !evidence.is_empty() {
            body.push_str("\n\nLast transcript lines:");
            for line in evidence {
                body.push_str("\n    ");
                body.push_str(&line);
            }
        }
        body
    }
}

fn stop_line(line: &str, count: usize) -> String {
    const MAX_CHARS: usize = 240;
    let mut cleaned = String::new();
    for word in line.split_whitespace() {
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        let bare = word.trim_start_matches(['\'', '"', '`', '(', '[', '{']);
        if let Some(path) = bare.strip_prefix('/')
            && !path.starts_with('/')
        {
            let end = bare.trim_end_matches(['\'', '"', '`', ')', ']', '}', ':', ',', ';', '.']);
            cleaned.push_str(&word[..word.len() - bare.len()]);
            cleaned.push_str("‹host path›");
            cleaned.push_str(&bare[end.len()..]);
        } else {
            cleaned.push_str(word);
        }
    }
    let mut line = cleaned.chars().take(MAX_CHARS).collect::<String>();
    if count > 1 {
        line.push_str(&format!(" ×{count}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{AgentIdDomain, ChatSeq};

    use super::*;

    #[test]
    fn stop_mail_folds_the_whole_run_and_does_not_repeat_its_reason() {
        let mut tail = StopTail::default();
        for line in [
            "cargo exited 101",
            "cargo exited 101",
            "compiling src/clock.rs",
            "read /tmp/private/clock.rs: denied",
            "read /tmp/private/clock.rs: denied",
            "read /tmp/private/clock.rs: denied",
            "see https://example.invalid/log",
            "test started",
            "test started",
        ] {
            assert!(tail.push(line));
        }
        assert!(!tail.push("too old"));
        assert_eq!(
            tail.body(Stop::Failed, "cargo exited 101", false),
            "failed · cargo exited 101 ×2\n\nLast transcript lines:\n    test started ×2\n    see https://example.invalid/log\n    read ‹host path›: denied ×3\n    compiling src/clock.rs"
        );
    }

    #[test]
    fn a_launch_failure_explains_recovery_even_without_other_transcript_lines() {
        let error =
            "capture Claude mount /tmp/fixture/claude-accounts/default: Operation not permitted";
        let mut tail = StopTail::default();
        tail.push(error);
        assert_eq!(
            tail.body(Stop::Failed, error, true),
            "failed · capture Claude mount ‹host path›: Operation not permitted\n\nThis agent cannot start. Replies are saved. Change its runtime or start another agent."
        );
    }

    #[test]
    fn newest_status_replaces_only_its_own_thread() {
        let mail = Mail::default();
        let mut changed = mail.status_changed.subscribe();
        let owner = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let a = MessageId(ChatSeq(7));
        let b = MessageId(ChatSeq(9));
        mail.set_status(a, owner, "Read the logs".into(), UnixMs(15))
            .unwrap();
        mail.set_status(b, owner, "Bisecting".into(), UnixMs(24))
            .unwrap();
        mail.set_status(a, owner, "Runs with SSL".into(), UnixMs(31))
            .unwrap();
        assert!(changed.has_changed().unwrap());
        changed.borrow_and_update();
        assert!(
            mail.set_status(a, owner, "one\ntwo".into(), UnixMs(33))
                .is_err()
        );
        assert!(!changed.has_changed().unwrap());
        {
            let held = mail.statuses.lock().unwrap();
            assert_eq!(held.len(), 2);
            assert_eq!(
                (&*held[&a].text, held[&a].at),
                ("Runs with SSL", UnixMs(31))
            );
            assert_eq!((&*held[&b].text, held[&b].at), ("Bisecting", UnixMs(24)));
        }
        mail.set_status(a, owner, String::new(), UnixMs(37))
            .unwrap();
        assert!(changed.has_changed().unwrap());
        let held = mail.statuses.lock().unwrap();
        assert_eq!(held.keys().copied().collect::<Vec<_>>(), [b]);
        assert!(Mail::default().statuses.lock().unwrap().is_empty());
    }

    #[test]
    fn a_stop_clears_only_status_still_owned_by_the_stopped_agent() {
        let mail = Mail::default();
        let a = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let b = AgentId::from_counter(2, &AgentIdDomain(0)).unwrap();
        let thread = MessageId(ChatSeq(7));
        let mut changed = mail.status_changed.subscribe();

        mail.set_status(thread, a, "Checking the clock".into(), UnixMs(31))
            .unwrap();
        changed.borrow_and_update();
        mail.clear_stopped_status(a);
        assert!(mail.status_snapshot().is_empty());
        assert!(changed.has_changed().unwrap());

        mail.set_status(thread, a, "Retrying the check".into(), UnixMs(35))
            .unwrap();
        mail.set_status(thread, b, "Verifying the result".into(), UnixMs(40))
            .unwrap();
        changed.borrow_and_update();
        mail.clear_stopped_status(a);
        assert_eq!(
            mail.status_snapshot(),
            [ThreadStatus {
                thread,
                at: UnixMs(40),
                text: "Verifying the result".into(),
            }]
        );
        assert!(!changed.has_changed().unwrap());

        mail.set_status(thread, b, "Writing the proof".into(), UnixMs(46))
            .unwrap();
        assert_eq!(mail.status_snapshot()[0].text, "Writing the proof");
        assert!(changed.has_changed().unwrap());
    }

    #[test]
    fn working_fills_only_missing_statuses_and_uses_the_newest_worker_clock() {
        let a = MessageId(ChatSeq(7));
        let b = MessageId(ChatSeq(4));
        let explicit = ThreadStatus {
            thread: a,
            at: UnixMs(31),
            text: "Checking the clock".into(),
        };
        let working = [(a, UnixMs(42)), (b, UnixMs(73)), (b, UnixMs(55))];
        assert_eq!(
            with_working(vec![explicit.clone()], working),
            [
                explicit,
                ThreadStatus {
                    thread: b,
                    at: UnixMs(73),
                    text: "working".into()
                },
            ]
        );
        let retry = Doing {
            activity: Activity::Retrying,
            since: UnixMs(10),
        };
        let deadline = retry.stuck_at().unwrap();
        assert!(is_working(retry, UnixMs(deadline.0 - 1)));
        assert!(!is_working(retry, deadline));
        assert!(!is_working(
            Doing {
                activity: Activity::Idle,
                ..retry
            },
            UnixMs(15)
        ));
    }

    #[test]
    fn cold_notice_is_once_per_thread_and_resets_after_a_return() {
        let mut presence = Presence::default();
        let thread = MessageId(ChatSeq(11));
        let elsewhere = MessageId(ChatSeq(12));
        let first = UnixMs(23);
        let edge = first + COLD_AFTER;
        assert_eq!(presence.cold(thread, edge, first), None);
        assert_eq!(presence.cold(thread, UnixMs(edge.0 + 1), first), Some(10));
        assert_eq!(presence.cold(thread, UnixMs(edge.0 + 183_000), first), None);

        let opened = UnixMs(edge.0 + 200_000);
        presence.warm(thread, opened).open += 2;
        let leaves = UnixMs(opened.0 + 1_200_000);
        presence.warm(thread, leaves).open -= 1;
        assert_eq!(
            presence.cold(thread, leaves + COLD_AFTER + Duration::from_secs(10), first),
            None
        );
        assert_eq!(presence.cold(elsewhere, leaves, first), Some(33));
        let last_left = UnixMs(leaves.0 + 700_000);
        presence.warm(thread, last_left).open -= 1;
        assert_eq!(presence.cold(thread, last_left + COLD_AFTER, first), None);
        assert_eq!(
            presence.cold(
                thread,
                last_left + COLD_AFTER + Duration::from_millis(1),
                first
            ),
            Some(10)
        );
    }
}
