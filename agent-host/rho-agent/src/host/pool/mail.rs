//! Mail state that is deliberately not in the chat log.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rho_agent_types::{Address, AgentId, Doing, Message, MessageId, Stop, UnixMs};
use tokio::sync::watch;

use super::AgentPool;
use crate::record::Event;
use crate::store::{ChatRead as _, ChatWrite as _, StoreRead as _};
use crate::{InferenceState, RuntimeState};

const COLD_AFTER: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
pub(super) struct Mail {
    statuses: Mutex<BTreeMap<MessageId, rho_agent_types::ThreadStatus>>,
    status_changed: watch::Sender<()>,
    presence: Mutex<Presence>,
    retry_checks: Mutex<HashMap<AgentId, tokio::task::JoinHandle<()>>>,
}

impl Mail {
    fn set_status(&self, thread: MessageId, text: String, at: UnixMs) -> anyhow::Result<()> {
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
            held.insert(thread, rho_agent_types::ThreadStatus { thread, at, text });
        }
        self.status_changed.send_replace(());
        Ok(())
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

    pub fn thread_statuses(&self) -> Vec<rho_agent_types::ThreadStatus> {
        self.mail
            .statuses
            .lock()
            .expect("poison")
            .values()
            .cloned()
            .collect()
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
        self.mail.set_status(thread, text, UnixMs::now())
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
            tokio::spawn(send_stop(weak, agent, doing, stop, reason));
        } else if let Some(deadline) = doing.stuck_at() {
            let check = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(deadline.0.saturating_sub(now.0))).await;
                let Some(pool) = weak.upgrade() else { return };
                {
                    let mut checks = pool.mail.retry_checks.lock().expect("poison");
                    if pool.doing.lock().expect("poison").get(&agent) != Some(&doing) {
                        return;
                    }
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
    let (thread, body) = {
        let read = pool.db.read();
        let thread = read
            .first_thread(agent)
            .or_else(|| origin(&read, agent).map(|mail| read.mail_thread(&mail)));
        let Some(thread) = thread else { return };
        let body = stop_body(&read, agent, stop, &reason);
        (thread, body)
    };
    let mut write = pool.db.write().await;
    let sender = Address::System {
        agent,
        stop,
        since: doing.since,
    };
    match write.post_stop(UnixMs::now(), thread, sender, body) {
        Ok(Some(_)) => write.commit(),
        Ok(None) => {}
        Err(error) => eprintln!("rho-agent: post stop of {}: {error:#}", agent.encoded()),
    }
}

fn stop_body(read: &rho_db::ReadTxn, agent: AgentId, stop: Stop, reason: &str) -> String {
    const MAX_LINES: usize = 5;
    const MAX_CHARS: usize = 240;
    let reason_line = rho_agent_types::one_line(reason)
        .chars()
        .take(MAX_CHARS)
        .collect::<String>();
    let mut body = format!("{} · {reason_line}", stop.word());
    let mut lines = Vec::new();
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
            Event::Notice {
                notice: crate::entry::Notice::Error(error) | crate::entry::Notice::Stopped(error),
                ..
            } => vec![error],
            _ => Vec::new(),
        };
        for line in fragments
            .iter()
            .rev()
            .flat_map(|fragment| fragment.lines().rev())
        {
            if line.trim().is_empty() {
                continue;
            }
            lines.push(line.chars().take(MAX_CHARS).collect::<String>());
            if lines.len() == MAX_LINES {
                break;
            }
        }
        if lines.len() == MAX_LINES {
            break;
        }
    }
    if !lines.is_empty() {
        body.push_str("\n\nLast transcript lines:");
        for line in lines.into_iter().rev() {
            body.push_str("\n    ");
            body.push_str(&line);
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use rho_agent_types::ChatSeq;

    use super::*;

    #[test]
    fn newest_status_replaces_only_its_own_thread() {
        let mail = Mail::default();
        let mut changed = mail.status_changed.subscribe();
        let a = MessageId(ChatSeq(7));
        let b = MessageId(ChatSeq(9));
        mail.set_status(a, "Read the logs".into(), UnixMs(15))
            .unwrap();
        mail.set_status(b, "Bisecting".into(), UnixMs(24)).unwrap();
        mail.set_status(a, "Runs with SSL".into(), UnixMs(31))
            .unwrap();
        assert!(changed.has_changed().unwrap());
        changed.borrow_and_update();
        assert!(mail.set_status(a, "one\ntwo".into(), UnixMs(33)).is_err());
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
        mail.set_status(a, String::new(), UnixMs(37)).unwrap();
        assert!(changed.has_changed().unwrap());
        let held = mail.statuses.lock().unwrap();
        assert_eq!(held.keys().copied().collect::<Vec<_>>(), [b]);
        assert!(Mail::default().statuses.lock().unwrap().is_empty());
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
