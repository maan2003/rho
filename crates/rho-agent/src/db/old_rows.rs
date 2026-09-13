//! One-hop migration (13 Sep): rows no build writes any more are
//! rewritten in place, so their variants can go. `WorkdirAdded` becomes
//! an empty `Notice`, which is what the mirror already made of it and
//! what the fold says nothing for. `ClaudePresentationSource`, the Claude
//! runtime's row shape before `Transcript` (6 Sep), becomes the
//! `Transcript` row it would be written as today: a user line for the
//! person's and an agent's words, an assistant line for the model's.
//! Positions are kept: the journal, rewinds, and every client's cursor
//! count them. Remove once the databases of interest have run it; the
//! two variants go with it.

use std::borrow::Cow;

use redb::TableDefinition;
use rho_core::AgentId;
use rho_db::{SenValue, WriteTxn};
use senax_encoder::{Decoder as _, Encoder as _};

use super::AGENT_LOG;
use crate::{AgentEvent, PresentationSpeaker, TranscriptLine};

pub(super) const FROM: &str = "6d0f41b9";
pub(super) const TO: &str = "b4e2c7a1";

/// The log's rows as bytes, under the name redb recorded for the typed
/// table: a store holds over a million rows and only a few are of the
/// shapes rewritten, so the scan reads the variant tag senax puts first
/// and decodes nothing else.
#[derive(Debug)]
struct RawRow;

impl redb::Value for RawRow {
    type SelfType<'a>
        = &'a [u8]
    where
        Self: 'a;
    type AsBytes<'a>
        = &'a [u8]
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> &'a [u8]
    where
        Self: 'a,
    {
        data
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a &'b [u8]) -> &'a [u8]
    where
        Self: 'b,
    {
        value
    }

    fn type_name() -> redb::TypeName {
        <rho_db::Sen<AgentEvent<'static>> as redb::Value>::type_name()
    }
}

const RAW_LOG: TableDefinition<(AgentId, u64), RawRow> = TableDefinition::new("agent_log");

/// What every encoding of `sample`'s variant starts with: the enum tag
/// and the variant id (one byte, or a marker and eight).
fn variant_prefix(sample: &AgentEvent<'_>) -> Vec<u8> {
    let mut bytes = bytes::BytesMut::new();
    sample.encode(&mut bytes).expect("encode a sample event");
    let id_len = if bytes.get(1) == Some(&0xFF) { 9 } else { 1 };
    bytes[..1 + id_len].to_vec()
}

pub(super) fn run(write: &mut WriteTxn) {
    let workdir_added = variant_prefix(&AgentEvent::WorkdirAdded {
        at: rho_core::UnixMs(0),
    });
    let presentation_source = variant_prefix(&AgentEvent::ClaudePresentationSource {
        source_id: uuid::Uuid::nil(),
        speaker: PresentationSpeaker::User,
        text: Cow::Borrowed(""),
        at: rho_core::UnixMs(0),
    });
    let rewrites: Vec<((AgentId, u64), AgentEvent<'static>)> = {
        let raw = write.open_table(RAW_LOG);
        raw.iter()
            .filter_map(|(key, value)| {
                let mut bytes = value.value();
                if !bytes.starts_with(&workdir_added) && !bytes.starts_with(&presentation_source) {
                    return None;
                }
                let event = AgentEvent::decode(&mut bytes).expect("senax decode agent log row");
                Some((key.value(), rewrite(event)?))
            })
            .collect()
    };
    let mut log = write.open_table(AGENT_LOG);
    for (key, event) in &rewrites {
        log.insert(key, SenValue::borrowed(event));
    }
    eprintln!(
        "rho-agent: {} rows of shapes no build writes are rewritten in place",
        rewrites.len()
    );
}

fn rewrite(event: AgentEvent<'static>) -> Option<AgentEvent<'static>> {
    Some(match event {
        AgentEvent::WorkdirAdded { at } => AgentEvent::Notice {
            text: Cow::Borrowed(""),
            at,
        },
        AgentEvent::ClaudePresentationSource {
            source_id,
            speaker,
            text,
            at,
        } => AgentEvent::Transcript {
            uuid: source_id,
            line: match speaker {
                PresentationSpeaker::User | PresentationSpeaker::Agent => TranscriptLine::User {
                    text: text.into_owned(),
                },
                PresentationSpeaker::Assistant => TranscriptLine::Assistant {
                    text: text.into_owned(),
                    calls: Vec::new(),
                    usage: None,
                    context_used: None,
                },
            },
            at,
            wake: None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_db::RhoDb;
    use rho_fs_view::{Place, WorksetMode};
    use uuid::Uuid;

    use super::*;
    use crate::db::{
        AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentRole, AgentRuntime,
        AgentWriteTxnExt as _, FORMAT, PromptCacheKey, SessionBinding,
    };

    #[tokio::test]
    async fn old_rows_are_rewritten_in_place_and_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("rho.redb"));
        let said = Uuid::new_v4();
        let replied = Uuid::new_v4();
        let mailed = Uuid::new_v4();
        let agent_id = {
            let mut write = db.write().await;
            write.init_agent_tables();
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
            write.append_agent_event(agent_id, &AgentEvent::WorkdirAdded { at: UnixMs(2) });
            write.append_agent_event(
                agent_id,
                &AgentEvent::ClaudePresentationSource {
                    source_id: said,
                    speaker: PresentationSpeaker::User,
                    text: Cow::Borrowed("hello"),
                    at: UnixMs(3),
                },
            );
            write.append_agent_event(
                agent_id,
                &AgentEvent::ClaudePresentationSource {
                    source_id: replied,
                    speaker: PresentationSpeaker::Assistant,
                    text: Cow::Borrowed("hi"),
                    at: UnixMs(4),
                },
            );
            write.append_agent_event(
                agent_id,
                &AgentEvent::ClaudePresentationSource {
                    source_id: mailed,
                    speaker: PresentationSpeaker::Agent,
                    text: Cow::Borrowed("mail"),
                    at: UnixMs(5),
                },
            );
            write.append_agent_event(
                agent_id,
                &AgentEvent::Notice {
                    text: Cow::Borrowed("still pending"),
                    at: UnixMs(6),
                },
            );
            // As a store the old build left: one hop behind.
            write.open_table(FORMAT).insert(&(), &FROM.to_owned());
            write.commit();
            agent_id
        };

        {
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
        }
        let read = db.read();
        assert_eq!(read.open_table(FORMAT).get(&()).unwrap().value(), TO);
        let (_, events) = read.agent_events(agent_id);
        assert_eq!(events.len(), 6);
        assert!(matches!(events[0], AgentEvent::Created { .. }));
        assert_eq!(
            events[1],
            AgentEvent::Notice {
                text: Cow::Borrowed(""),
                at: UnixMs(2)
            }
        );
        assert_eq!(
            events[2],
            AgentEvent::Transcript {
                uuid: said,
                line: TranscriptLine::User {
                    text: "hello".to_owned()
                },
                at: UnixMs(3),
                wake: None,
            }
        );
        assert_eq!(
            events[3],
            AgentEvent::Transcript {
                uuid: replied,
                line: TranscriptLine::Assistant {
                    text: "hi".to_owned(),
                    calls: Vec::new(),
                    usage: None,
                    context_used: None,
                },
                at: UnixMs(4),
                wake: None,
            }
        );
        assert!(matches!(
            &events[4],
            AgentEvent::Transcript {
                uuid,
                line: TranscriptLine::User { text },
                ..
            } if *uuid == mailed && text == "mail"
        ));
        assert_eq!(
            events[5],
            AgentEvent::Notice {
                text: Cow::Borrowed("still pending"),
                at: UnixMs(6)
            }
        );
        // The rewritten user lines carried nothing; the real notice after
        // them is what is pending, and an empty one never is.
        let head = read.get_agent(agent_id);
        assert_eq!(head.pending_notice.as_deref(), Some("still pending"));
        assert!(head.user_interacted);
    }
}
