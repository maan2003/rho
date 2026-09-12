//! One-hop migration (13 Sep): an agent's `Created` row names one place in
//! a workset instead of a list of workdirs, and agents from before
//! worksets get a place of their own. Every `Created` row is rewritten;
//! an agent whose first workdir was not in a workset gets an empty one
//! (exposed, as a checkout on the host was), and a `Notice` at the tail of
//! its log says where its old checkout is, for its next user message to
//! carry. Nothing is copied. Remove once the databases of interest have
//! run a build with it; `Notice` rows are ordinary data.

use std::borrow::Cow;

use camino::Utf8PathBuf;
use prefix_id::{PrefixId, PrefixIdDomain};
use redb::TableDefinition;
use rho_core::{AgentId, UnixMs};
use rho_db::{RecordedTypeName, SenAs, SenValue, WriteTxn};
use rho_fs_view::{Place, WorksetMode};
use senax_encoder::{Decode, Encode};

use super::{
    AGENT_LOG, AgentRole, AgentRuntime, AgentSpawnedBy, AgentWriteTxnExt as _, SessionBinding,
};
use crate::AgentEvent;

pub(super) const FROM: &str = "3ac1e7d4";
pub(super) const TO: &str = "203a7685";

/// The log's rows as the old build wrote `Created`, the one variant this
/// reads: row zero of every agent, never anything else.
#[derive(Clone, Debug, Encode, Decode)]
enum OldEvent {
    Created {
        role: AgentRole,
        binding: SessionBinding,
        runtime: AgentRuntime,
        workdirs: Vec<OldWorkspaceInfo>,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        created_at: UnixMs,
        #[senax(default)]
        parent: Option<AgentId>,
    },
}

#[derive(Clone, Debug, Encode, Decode)]
enum OldWorkspaceInfo {
    UserCheckout {
        repo: Utf8PathBuf,
    },
    Workspace {
        repo: Utf8PathBuf,
        #[senax(rename = "name")]
        id: PrefixId<OldWorkspaceIdDomain>,
    },
    Sandbox {
        repo: Utf8PathBuf,
        id: PrefixId<OldWorkspaceIdDomain>,
    },
    Workset {
        workset: String,
        cwd: Utf8PathBuf,
        #[senax(default)]
        mode: WorksetMode,
        #[senax(default)]
        origin: Option<Utf8PathBuf>,
    },
}

/// The id family of the jj managed workspaces of the time; only its
/// encoding is read here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct OldWorkspaceIdDomain;

impl PrefixIdDomain for OldWorkspaceIdDomain {
    const KIND: &'static str = "managed-workspace-id";

    fn machine_seed(&self) -> u64 {
        0
    }
}

/// The log table under the value type name redb recorded for it, read
/// with the old row shape.
#[derive(Debug)]
struct AgentLogName;

impl RecordedTypeName for AgentLogName {
    const NAME: &'static str = "rho-db::Sen<rho_agent::AgentEvent<'_>>";
}

const OLD_LOG: TableDefinition<(AgentId, u64), SenAs<OldEvent, AgentLogName>> =
    TableDefinition::new("agent_log");

pub(super) fn run(write: &mut WriteTxn) {
    let mut rewrites = Vec::new();
    {
        let old = write.open_table(OLD_LOG);
        // One descent per agent: the first row is the creation, and the
        // next agent's first row is right after this agent's last key.
        let mut cursor = old.iter().next().map(|(key, _)| key.value().0);
        while let Some(agent_id) = cursor {
            if let Some(row) = old.get(&(agent_id, 0)) {
                let OldEvent::Created {
                    role,
                    binding,
                    runtime,
                    workdirs,
                    spawned_by,
                    spawn_name,
                    created_at,
                    parent,
                } = row.value().into_owned();
                let (place, notice) = match workdirs.into_iter().next() {
                    Some(OldWorkspaceInfo::Workset {
                        workset,
                        cwd,
                        mode,
                        origin,
                    }) => (
                        Place {
                            workset,
                            cwd,
                            mode,
                            origin,
                        },
                        None,
                    ),
                    Some(old) => (
                        Place {
                            workset: mint_workset_id(),
                            cwd: Utf8PathBuf::from(rho_fs_view::MOUNT_ROOT),
                            mode: WorksetMode::Exposed,
                            origin: None,
                        },
                        Some(moved_notice(&old)),
                    ),
                    None => panic!("agent {agent_id:?} was created with no workdir"),
                };
                let created = AgentEvent::Created {
                    role,
                    binding,
                    runtime,
                    place,
                    spawned_by,
                    spawn_name,
                    created_at,
                    parent,
                };
                rewrites.push((agent_id, created, notice));
            }
            cursor = old
                .range((agent_id, u64::MAX)..)
                .next()
                .map(|(key, _)| key.value().0)
                .filter(|next| *next != agent_id);
        }
    }
    let moved = rewrites
        .iter()
        .filter(|(_, _, notice)| notice.is_some())
        .count();
    for (agent_id, created, notice) in &rewrites {
        write
            .open_table(AGENT_LOG)
            .insert(&(*agent_id, 0), SenValue::borrowed(created));
        if let Some(text) = notice {
            write.append_agent_event(
                *agent_id,
                &AgentEvent::Notice {
                    text: Cow::Borrowed(text),
                    at: UnixMs::now(),
                },
            );
        }
    }
    eprintln!(
        "rho-agent: {} agents' creations now name a place; {moved} from before worksets got an empty one",
        rewrites.len()
    );
}

/// A new workset id, the shape `rho-fs-view` makes: twelve hex digits.
fn mint_workset_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

/// What the agent is told, ahead of its first user message after the move.
fn moved_notice(old: &OldWorkspaceInfo) -> String {
    let mut note = String::from(
        "Note from Rho: this agent was moved into a workset. Your working directory is now \
         /src, an empty directory of your own; the rest of the host is visible as before. \
         Nothing was copied. ",
    );
    match old {
        OldWorkspaceInfo::Workspace { repo, id } | OldWorkspaceInfo::Sandbox { repo, id } => {
            let workspace = format!("ws-{}", id.encoded());
            note.push_str(&format!(
                "Before, you worked in the jj workspace {workspace} of {repo}, which is still \
                 there: `jj -R {repo} --ignore-working-copy log -r '{workspace}@'` shows its \
                 working-copy commit and `jj -R {repo} --ignore-working-copy git root` its git \
                 store. Clone the repository you need into /src (`git clone <url> /src/<name>`) \
                 and bring over what you want, for example \
                 `git -C /src/<name> -c uploadpack.allowAnySHA1InWant=true fetch <git store> <commit>` \
                 and then a checkout or cherry-pick of it. Leave the old workspace as it is."
            ));
        }
        OldWorkspaceInfo::UserCheckout { repo } => {
            note.push_str(&format!(
                "Before, you worked directly in {repo}, the user's own checkout. Clone the \
                 repository you need into /src (`git clone <url> /src/<name>`) and continue \
                 there; leave {repo} to the user."
            ));
        }
        OldWorkspaceInfo::Workset { workset, cwd, .. } => {
            note.push_str(&format!(
                "Before, you worked at {cwd} in workset {workset}."
            ));
        }
    }
    note
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;

    use super::*;
    use crate::db::{AgentReadTxnExt as _, FORMAT, PromptCacheKey};

    #[test]
    fn the_log_is_opened_under_the_name_redb_recorded() {
        assert_eq!(
            <rho_db::Sen<AgentEvent<'static>> as redb::Value>::type_name().name(),
            AgentLogName::NAME
        );
    }

    #[tokio::test]
    async fn creations_are_rewritten_and_the_moved_get_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("rho.redb"));
        let (in_workset, legacy) = {
            let mut write = db.write().await;
            write.init_agent_tables();
            let in_workset = write.alloc_agent_id();
            let legacy = write.alloc_agent_id();
            let created = |workdir: OldWorkspaceInfo| OldEvent::Created {
                role: AgentRole::default(),
                binding: SessionBinding::ResponsesSol(Default::default()),
                runtime: AgentRuntime::Rho {
                    prompt_cache_key: PromptCacheKey::generate(),
                },
                workdirs: vec![workdir],
                spawned_by: AgentSpawnedBy::Direct,
                spawn_name: Some("worker".to_owned()),
                created_at: UnixMs(1),
                parent: None,
            };
            {
                let mut old = write.open_table(OLD_LOG);
                old.insert(
                    &(in_workset, 0),
                    SenValue::borrowed(&created(OldWorkspaceInfo::Workset {
                        workset: "0123456789ab".into(),
                        cwd: "/src/rho".into(),
                        mode: WorksetMode::View,
                        origin: Some("https://example.test/rho.git".into()),
                    })),
                );
                old.insert(
                    &(legacy, 0),
                    SenValue::borrowed(&created(OldWorkspaceInfo::Workspace {
                        repo: "/home/someone/src/rho".into(),
                        id: PrefixId::from_counter(7, &OldWorkspaceIdDomain).unwrap(),
                    })),
                );
            }
            write.open_table(FORMAT).insert(&(), &FROM.to_owned());
            write.commit();
            (in_workset, legacy)
        };

        {
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
        }
        let read = db.read();
        assert_eq!(read.open_table(FORMAT).get(&()).unwrap().value(), TO);
        let kept = read.get_agent(in_workset);
        assert_eq!(
            kept.config.place,
            Place {
                workset: "0123456789ab".into(),
                cwd: "/src/rho".into(),
                mode: WorksetMode::View,
                origin: Some("https://example.test/rho.git".into()),
            }
        );
        assert_eq!(kept.config.spawn_name.as_deref(), Some("worker"));
        assert_eq!(kept.pending_notice, None);
        let moved = read.get_agent(legacy);
        assert_eq!(moved.config.place.cwd, "/src");
        assert_eq!(moved.config.place.mode, WorksetMode::Exposed);
        assert_eq!(moved.config.place.workset.len(), 12);
        let notice = moved.pending_notice.expect("the moved agent has a notice");
        assert!(notice.contains("/home/someone/src/rho"), "{notice}");
        assert!(notice.contains("ws-"), "{notice}");
        assert_eq!(read.agent_events(legacy).1.len(), 2);
    }
}
