//! One-hop migration (13 Sep, later the same day): the notice `places`
//! left for a moved agent told it to clone the local git store; the
//! repository's remote is the thing to clone, and a wrong URL sends an
//! agent's work to the wrong place. Every agent whose moved notice is
//! still pending (no user message has carried it yet) gets a `Notice` at
//! its tail in the words `places` writes now, with the workspace and
//! repository read out of the old one; the fold takes the newest notice,
//! so the old row stays and is never said. An agent already told is left
//! alone. Remove with `places`.

use std::borrow::Cow;

use camino::Utf8Path;
use rho_core::{AgentId, UnixMs};
use rho_db::WriteTxn;

use super::places::{self, MovedFrom};
use super::{AGENT_LOG, AgentWriteTxnExt as _, agent_range, carries_notice, rows};
use crate::AgentEvent;

pub(super) const FROM: &str = places::TO;
pub(super) const TO: &str = "6d0f41b9";

/// What every old moved notice said, and no new one does.
const OLD_WORDING: &str = "Clone the repository you need into /src (`git clone <url> /src/<name>`)";

pub(super) fn run(write: &mut WriteTxn) {
    let mut renewals: Vec<(AgentId, String)> = Vec::new();
    {
        let log = write.open_table(AGENT_LOG);
        let mut cursor = log.iter().next().map(|(key, _)| key.value().0);
        while let Some(agent_id) = cursor {
            // Newest row first: a notice is pending until a user message
            // follows it, and a later notice replaces an earlier one.
            for (_, event) in rows(log.range(agent_range(agent_id)).rev()) {
                if carries_notice(&event) {
                    break;
                }
                if let AgentEvent::Notice { text, .. } = &event {
                    if let Some(renewed) = renewed_moved_notice(text) {
                        renewals.push((agent_id, renewed));
                    }
                    break;
                }
            }
            cursor = log
                .range((agent_id, u64::MAX)..)
                .next()
                .map(|(key, _)| key.value().0)
                .filter(|next| *next != agent_id);
        }
    }
    for (agent_id, text) in &renewals {
        write.append_agent_event(
            *agent_id,
            &AgentEvent::Notice {
                text: Cow::Borrowed(text),
                at: UnixMs::now(),
            },
        );
    }
    eprintln!(
        "rho-agent: {} moved agents' pending notices now name the remote to clone",
        renewals.len()
    );
}

/// The notice in today's words, if `old` is a moved notice in yesterday's.
fn renewed_moved_notice(old: &str) -> Option<String> {
    if !old.contains(OLD_WORDING) {
        return None;
    }
    if let Some((_, rest)) = old.split_once("Before, you worked in the jj workspace ") {
        let (workspace, rest) = rest.split_once(" of ")?;
        let (repo, _) = rest.split_once(", which is still there")?;
        return Some(places::moved_notice_text(&MovedFrom::Workspace {
            repo: Utf8Path::new(repo),
            workspace: workspace.to_owned(),
        }));
    }
    if let Some((_, rest)) = old.split_once("Before, you worked directly in ") {
        let (repo, _) = rest.split_once(", the user's own checkout")?;
        return Some(places::moved_notice_text(&MovedFrom::UserCheckout {
            repo: Utf8Path::new(repo),
        }));
    }
    None
}

#[cfg(test)]
mod tests {
    use rho_core::{ContentPart, MessageSender};
    use rho_db::RhoDb;
    use rho_fs_view::{Place, WorksetMode};

    use super::*;
    use crate::db::{
        AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentRole, AgentRuntime, FORMAT,
        PromptCacheKey, SessionBinding,
    };
    use crate::{InputKind, MessageDelivery, QueuedInput};

    fn old_workspace_notice(workspace: &str, repo: &str) -> String {
        format!(
            "Note from Rho: this agent was moved into a workset. Your working directory is now \
             /src, an empty directory of your own; the rest of the host is visible as before. \
             Nothing was copied. Before, you worked in the jj workspace {workspace} of {repo}, \
             which is still there: `jj -R {repo} --ignore-working-copy log -r '{workspace}@'` \
             shows its working-copy commit and `jj -R {repo} --ignore-working-copy git root` \
             its git store. {OLD_WORDING} and bring over what you want, for example \
             `git -C /src/<name> -c uploadpack.allowAnySHA1InWant=true fetch <git store> <commit>` \
             and then a checkout or cherry-pick of it. Leave the old workspace as it is."
        )
    }

    fn old_checkout_notice(repo: &str) -> String {
        format!(
            "Note from Rho: this agent was moved into a workset. Your working directory is now \
             /src, an empty directory of your own; the rest of the host is visible as before. \
             Nothing was copied. Before, you worked directly in {repo}, the user's own checkout. \
             {OLD_WORDING} and continue there; leave {repo} to the user."
        )
    }

    fn notice(text: String) -> AgentEvent<'static> {
        AgentEvent::Notice {
            text: Cow::Owned(text),
            at: UnixMs(2),
        }
    }

    fn user_message() -> AgentEvent<'static> {
        AgentEvent::Accepted(QueuedInput {
            source: MessageSender::User,
            kind: InputKind::Message {
                content: vec![ContentPart::Text {
                    text: "hello".to_owned(),
                }],
            },
            delivery: MessageDelivery::NextRequest,
            at: UnixMs(3),
        })
    }

    #[tokio::test]
    async fn pending_moved_notices_are_renewed_and_carried_ones_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("rho.redb"));
        let (pending, checkout, told, current) = {
            let mut write = db.write().await;
            write.init_agent_tables();
            let create = |write: &mut WriteTxn| {
                let agent_id = write.alloc_agent_id();
                write.create_agent(
                    UnixMs(1),
                    agent_id,
                    None,
                    Place {
                        workset: "0123456789ab".into(),
                        cwd: "/src".into(),
                        mode: WorksetMode::Exposed,
                        origin: None,
                    },
                    AgentRole::default(),
                    SessionBinding::ResponsesSol(Default::default()),
                    AgentRuntime::Rho {
                        prompt_cache_key: PromptCacheKey::generate(),
                    },
                    None,
                );
                agent_id
            };
            let pending = create(&mut write);
            let checkout = create(&mut write);
            let told = create(&mut write);
            let current = create(&mut write);
            write.append_agent_event(
                pending,
                &notice(old_workspace_notice("ws-abc123", "/home/someone/src/rho")),
            );
            write.append_agent_event(
                checkout,
                &notice(old_checkout_notice("/home/someone/src/rho")),
            );
            write.append_agent_event(
                told,
                &notice(old_workspace_notice("ws-def456", "/home/someone/src/rho")),
            );
            write.append_agent_event(told, &user_message());
            write.append_agent_event(
                current,
                &notice(places::moved_notice_text(&MovedFrom::Workspace {
                    repo: Utf8Path::new("/home/someone/src/rho"),
                    workspace: "ws-abc123".to_owned(),
                })),
            );
            // As a store the old build left: one hop behind.
            write.open_table(FORMAT).insert(&(), &FROM.to_owned());
            write.commit();
            (pending, checkout, told, current)
        };

        {
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
        }
        let read = db.read();
        assert_eq!(read.open_table(FORMAT).get(&()).unwrap().value(), TO);

        let renewed = read
            .get_agent(pending)
            .pending_notice
            .expect("still pending");
        assert!(renewed.contains("ws-abc123"), "{renewed}");
        assert!(renewed.contains("/home/someone/src/rho"), "{renewed}");
        assert!(renewed.contains("remote get-url origin"), "{renewed}");
        assert!(!renewed.contains(OLD_WORDING), "{renewed}");
        assert_eq!(read.agent_events(pending).1.len(), 3);

        let renewed = read
            .get_agent(checkout)
            .pending_notice
            .expect("still pending");
        assert!(renewed.contains("the user's own checkout"), "{renewed}");
        assert!(renewed.contains("remote get-url origin"), "{renewed}");
        assert_eq!(read.agent_events(checkout).1.len(), 3);

        // Told already: nothing pending, nothing appended.
        assert_eq!(read.get_agent(told).pending_notice, None);
        assert_eq!(read.agent_events(told).1.len(), 3);

        // Already in today's words: left alone.
        assert_eq!(read.agent_events(current).1.len(), 2);

        // A second open is a no-op.
        {
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
        }
        assert_eq!(db.read().agent_events(pending).1.len(), 3);
    }
}
