//! Mail state that is deliberately not in the chat log.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rho_agent_types::{
    Activity, Address, AgentId, ContentPart, Doing, FLASH_LIFETIME, Message, MessageId, Stop,
    ThreadWork, UnixMs, Voice, WorksetId,
};
use tokio::sync::watch;

use super::AgentPool;
use crate::record::{Event, Record};
use crate::store::{MailRead as _, MailWrite as _, NewMessage, StoreRead as _, StoreWrite as _};
use crate::{InferenceState, RuntimeState};

#[derive(Default)]
pub(super) struct Mail {
    /// Threads whose agent the user stopped, by that agent, until an agent
    /// works there again.
    stopped: Mutex<BTreeMap<MessageId, AgentId>>,
    work_changed: watch::Sender<()>,
    retry_checks: Mutex<HashMap<AgentId, tokio::task::JoinHandle<()>>>,
}

impl Mail {
    fn forget_stopped_agent(&self, agent: AgentId) {
        let mut stopped = self.stopped.lock().expect("poison");
        let before = stopped.len();
        stopped.retain(|_, owner| *owner != agent);
        if stopped.len() != before {
            self.work_changed.send_replace(());
        }
    }
}

impl AgentPool {
    pub(super) fn revise_agent_words(
        self: &Arc<Self>,
        mut write: rho_db::WriteTxn,
        workset: rho_agent_types::WorksetId,
        at: UnixMs,
        agent: AgentId,
        message: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()> {
        let before = write.mail_version(message);
        write.edit(at, agent, message, content)?;
        let version = write.mail_version(message);
        if version != before {
            write.append(
                workset,
                &Record {
                    at,
                    event: Event::MailWritten {
                        agent,
                        message,
                        version,
                    },
                },
            )?;
            write.commit();
        }
        Ok(())
    }

    pub(super) fn write_start_failure(
        &self,
        write: &mut rho_db::WriteTxn,
        origin: MessageId,
        text: String,
    ) -> anyhow::Result<()> {
        let thread = {
            let read = self.db.read();
            let mail = read
                .message(origin)
                .ok_or_else(|| anyhow::anyhow!("no mail #{}", origin.0.0))?;
            read.mail_thread(&mail)
        };
        write.post(
            UnixMs::now(),
            Some(thread),
            NewMessage {
                from: Address::StartFailed,
                voice: Voice::Message,
                content: vec![ContentPart::Text { text }],
            },
            None,
        )?;
        Ok(())
    }

    pub fn watch_thread_work(&self) -> watch::Receiver<()> {
        self.mail.work_changed.subscribe()
    }

    /// What the agents of each thread do now. A stuck agent does not
    /// work: its stop mail says so.
    pub fn thread_work(&self) -> Vec<(MessageId, ThreadWork)> {
        let at = UnixMs::now();
        let doing = self.doing.lock().expect("poison").clone();
        let read = self.db.read();
        let working = doing
            .into_iter()
            .filter(|(_, doing)| is_working(*doing, at))
            .filter_map(|(agent, _)| thread_for(&read, agent))
            .collect::<HashSet<_>>();
        let stopped = self.mail.stopped.lock().expect("poison").clone();
        let mut work = stopped
            .into_keys()
            // Another agent can still be working on the same thread.
            .filter(|thread| !working.contains(thread))
            .map(|thread| (thread, ThreadWork::StoppedByYou))
            .chain(
                working
                    .iter()
                    .copied()
                    .map(|thread| (thread, ThreadWork::Working)),
            )
            .collect::<Vec<_>>();
        work.sort_unstable_by_key(|(thread, _)| *thread);
        work
    }

    pub(super) fn user_stopped(&self, agent: AgentId) {
        if let Some(thread) = thread_for(&self.db.read(), agent) {
            self.mail
                .stopped
                .lock()
                .expect("poison")
                .insert(thread, agent);
            self.mail.work_changed.send_replace(());
        }
    }

    pub(super) fn activity_changed(&self, agent: AgentId, _before: Option<Doing>, now: Doing) {
        if is_working(now, UnixMs::now())
            && let Some(thread) = thread_for(&self.db.read(), agent)
        {
            self.mail.stopped.lock().expect("poison").remove(&thread);
        }
        self.thread_work_changed();
    }

    pub(super) fn agents_left_mail(&self, agents: &[AgentId]) {
        for &agent in agents {
            self.mail.forget_stopped_agent(agent);
        }
    }

    pub(crate) fn thread_work_changed(&self) {
        self.mail.work_changed.send_replace(());
    }

    /// The project the sender works in; children and advisors inherit
    /// their spawner's project.
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

    /// The project's message to the user in `thread`: one live message per
    /// agent since the user last wrote there, which the agent edits.
    pub(super) async fn send_chat_message(
        self: &Arc<Self>,
        workset: WorksetId,
        sender: AgentId,
        thread: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<MessageId> {
        self.own_agent(workset, sender)?;
        let mut write = self.db.write().await;
        let at = UnixMs::now();
        if let Some(live) = write.live_project_mail(thread, sender) {
            anyhow::bail!(
                "chat.send refused: you already sent message #{} since the user's last message in this thread. Use chat.edit({}, text), or use kind=\"flash\" for anything that does not need the user.",
                live.0.0,
                live.0.0,
            );
        }
        let id = write.post(
            at,
            Some(thread),
            NewMessage {
                from: Address::Agent(sender),
                voice: Voice::Message,
                content,
            },
            None,
        )?;
        record_agent_post(&mut write, workset, at, sender, id)?;
        write.commit();
        self.consider_naming(id);
        Ok(id)
    }

    /// One agent's words to another, under the sender's project; the user
    /// can read them there and is never dealt them.
    pub(super) async fn message_agent(
        self: &Arc<Self>,
        workset: WorksetId,
        sender: AgentId,
        to: AgentId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<MessageId> {
        self.own_agent(workset, sender)?;
        self.workset_of(to)?;
        anyhow::ensure!(sender != to, "cannot message yourself");
        let project = self.mail_project(sender);
        let mut write = self.db.write().await;
        let at = UnixMs::now();
        let id = write.post_peer(
            at,
            project,
            NewMessage {
                from: Address::Agent(sender),
                voice: Voice::ToAgent(to),
                content,
            },
        )?;
        record_agent_post(&mut write, workset, at, sender, id)?;
        write.commit();
        self.wake_receivers(id, Some(workset));
        Ok(id)
    }

    pub(super) async fn send_flash(
        self: &Arc<Self>,
        workset: WorksetId,
        sender: AgentId,
        thread: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<MessageId> {
        self.own_agent(workset, sender)?;
        let mut write = self.db.write().await;
        let at = UnixMs::now();
        let id = write.post_flash(at, thread, sender, content)?;
        record_agent_post(&mut write, workset, at, sender, id)?;
        write.commit();
        self.schedule_flash_expiry(id, at + FLASH_LIFETIME);
        Ok(id)
    }

    pub(super) async fn restore_flash_expiry(self: &Arc<Self>) {
        let deadlines = self.db.read().flash_deadlines();
        let now = UnixMs::now();
        let (ready, after): (Vec<_>, Vec<_>) = deadlines
            .into_iter()
            .partition(|(_, deadline)| *deadline <= now);
        if !ready.is_empty() {
            let mut write = self.db.write().await;
            let mut changed = false;
            for (id, _) in ready {
                changed |= write.expire_flash(now, id);
            }
            if changed {
                write.commit();
            }
        }
        for (id, deadline) in after {
            self.schedule_flash_expiry(id, deadline);
        }
    }

    fn schedule_flash_expiry(self: &Arc<Self>, id: MessageId, deadline: UnixMs) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let left = deadline.0.saturating_sub(UnixMs::now().0);
                if left != 0 {
                    tokio::time::sleep(Duration::from_millis(left)).await;
                    continue;
                }
                let Some(pool) = weak.upgrade() else { return };
                let mut write = pool.db.write().await;
                if write.expire_flash(UnixMs::now(), id) {
                    write.commit();
                }
                // If it is still the newest, the next post checks it.
                return;
            }
        });
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
                    pool.thread_work_changed();
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

fn record_agent_post(
    write: &mut rho_db::WriteTxn,
    workset: WorksetId,
    at: UnixMs,
    agent: AgentId,
    message: MessageId,
) -> anyhow::Result<()> {
    let version = write.mail_version(message);
    write.append(
        workset,
        &Record {
            at,
            event: Event::MailWritten {
                agent,
                message,
                version,
            },
        },
    )?;
    Ok(())
}

fn is_working(doing: Doing, now: UnixMs) -> bool {
    match doing.activity {
        Activity::Working => true,
        Activity::Retrying => doing.stop(now).is_none(),
        _ => false,
    }
}

fn thread_for(read: &rho_db::ReadTxn, agent: AgentId) -> Option<MessageId> {
    let head = read.agent(agent)?;
    read.message(MessageId(head.delivered))
        .or_else(|| origin(read, agent))
        .map(|mail| read.mail_thread(&mail))
        .or_else(|| read.first_thread(agent))
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
        let thread = if stop == Stop::Stuck {
            thread_for(&read, agent)
        } else {
            read.first_thread(agent)
                .or_else(|| origin(&read, agent).map(|mail| read.mail_thread(&mail)))
        };
        let Some(thread) = thread else { return };
        let failed_to_start = head
            .startup_failure
            .is_some_and(|failure| failure.since == doing.since);
        let body = stop_body(&read, agent, stop, &reason, failed_to_start);
        (head.workset, thread, body)
    };
    let mut write = pool.db.write().await;
    // Recovery can race the timer. Either the stop commits first and is
    // edited by recovery, or we already know the streak ended and post none.
    if stop == Stop::Stuck && pool.doing.lock().expect("poison").get(&agent) != Some(&doing) {
        return;
    }
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
                        event: Event::MailWritten {
                            agent,
                            message,
                            version: Some(message.0),
                        },
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
            body.push_str("\n\nThis agent cannot start. Reply to retry, change its runtime, or start another agent.");
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
            "failed · capture Claude mount ‹host path›: Operation not permitted\n\nThis agent cannot start. Reply to retry, change its runtime, or start another agent."
        );
    }

    #[test]
    fn a_retrying_agent_works_until_it_is_stuck() {
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
}
