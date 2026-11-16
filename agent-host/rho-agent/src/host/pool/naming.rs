//! The host's one naming attempt per thread.
//!
//! A thread under a root is what the GUI lists, so its name is what the
//! user reads first. Nothing names most of them: an agent rarely does, and
//! the user types a message, not a title. So when a thread gets its second
//! message, the host asks a model once for a name from those two, and
//! keeps it only if nothing named the thread meanwhile. A name given later
//! replaces it like any other
//! ([ARCH-agent-host](../../../../specs/ARCH-agent-host.md)).

use std::sync::Arc;
use std::time::Duration;

use rho_agent_types::{MessageId, UnixMs};

use super::AgentPool;
use crate::store::{ChatRead as _, ChatWrite as _};

const INSTRUCTIONS: &str = "You name chat threads. The input is the first two messages of a \
thread. Answer with a name for the thread: at most six words that say what it is about, in \
the language of the messages. Plain words only, with no quotes, no trailing period and no \
markdown. Answer with the name alone.";

/// A model that does not answer in this long has no name worth waiting
/// for: the thread already shows its first line.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Longer than this is not a name. The model is asked for six words; the
/// bound is against one that answers with a paragraph.
const MAX_CHARS: usize = 80;

impl AgentPool {
    /// `message` was just posted: when it is the second message of a
    /// thread under a root that nothing has named, start the one attempt.
    pub(super) fn consider_naming(self: &Arc<Self>, message: MessageId) {
        let read = self.db.read();
        let Some(reply) = read.message(message) else {
            return;
        };
        let Some(thread) = reply.parent else {
            return;
        };
        let Some(anchor) = read.message(thread) else {
            return;
        };
        let under_root = anchor
            .parent
            .and_then(|root| read.message(root))
            .is_some_and(|root| root.parent.is_none());
        if !under_root || read.thread_messages(Some(thread), 2).len() != 1 {
            return;
        }
        if read
            .thread(thread)
            .is_some_and(|head| head.name != anchor.headline())
        {
            return;
        }
        drop(read);
        let input = format!("{}\n\n{}", anchor.text(), reply.text());
        let pool = self.clone();
        tokio::spawn(async move {
            if let Err(error) = pool.name_thread(thread, input).await {
                eprintln!("rho-agent: naming #{}: {error:#}", thread.0.0);
            }
        });
    }

    async fn name_thread(&self, thread: MessageId, input: String) -> anyhow::Result<()> {
        let answer = tokio::time::timeout(
            TIMEOUT,
            self.inference.client().text(Arc::from(INSTRUCTIONS), input),
        )
        .await
        .map_err(|_| anyhow::anyhow!("no answer in {}s", TIMEOUT.as_secs()))??;
        let Some(name) = thread_name(&answer) else {
            anyhow::bail!("the answer is not a name: {answer:?}");
        };
        let mut write = self.db.write().await;
        if write.name_unnamed(UnixMs::now(), thread, name)? {
            write.commit();
        }
        Ok(())
    }
}

/// The name in a model's answer: its first line, without the quotes and
/// the period a model adds despite instructions. `None` for an answer that
/// is not a short line.
fn thread_name(answer: &str) -> Option<String> {
    let line = answer
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line = line
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*' || c == '#')
        .trim()
        .trim_end_matches('.')
        .trim();
    let name = line.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty() && name.chars().count() <= MAX_CHARS).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::thread_name;

    #[test]
    fn a_name_is_the_answers_first_line_bare() {
        assert_eq!(
            thread_name("\n  \"Fixing the  flaky test.\"  \nMore words\n"),
            Some("Fixing the flaky test".to_owned())
        );
        assert_eq!(
            thread_name("**Deploy plan**"),
            Some("Deploy plan".to_owned())
        );
        assert_eq!(thread_name("   \n\n"), None);
        assert_eq!(thread_name(&"word ".repeat(30)), None);
    }
}
