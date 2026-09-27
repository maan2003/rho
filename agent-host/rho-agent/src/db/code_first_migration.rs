//! Temporary a7e43d91 -> e31bcf82 migration to explicit messages. Remove after
//! the migrated build has opened active databases. Log positions are remapped
//! with their journal and rewind targets; the matching client mirror generation
//! refetches them.
use std::collections::BTreeMap;

use rho_agent_types::AgentId;
use rho_db::{SenValue, WriteTxn};

use super::{AGENT_HEADS, AGENT_LOG, AgentEventPos, JOURNAL, fold_head};
use crate::entry::{Block, Entry, MessageId, Party, Wake};
use crate::{AgentEvent, TranscriptLine};

pub(super) fn migrate(write: &mut WriteTxn) {
    // Only old native prose was user-facing. Prose from a code-first step is
    // deliberately private, even if a model ignored its tool instructions.
    let spoken = write
        .open_table(AGENT_LOG)
        .iter()
        .filter_map(|(key, row)| match row.value().into_owned() {
            AgentEvent::Native(crate::db::legacy::NativeEvent::ResponseFinished {
                usage, ..
            }) => Some((key.value(), usage)),
            _ => None,
        })
        .collect();
    super::entries_migration::migrate(write);
    rewrite(write, spoken);
}

fn rewrite(
    write: &mut WriteTxn,
    spoken: BTreeMap<(AgentId, u64), Option<super::AgentUsageBucket>>,
) {
    let old = write
        .open_table(AGENT_LOG)
        .iter()
        .map(|(key, row)| (key.value(), row.value().into_owned()))
        .collect::<Vec<_>>();
    let journal = write
        .open_table(JOURNAL)
        .iter()
        .map(|(_, row)| row.value())
        .collect::<Vec<_>>();
    let mut next = BTreeMap::<AgentId, u64>::new();
    let mut positions = BTreeMap::<(AgentId, u64), Vec<u64>>::new();
    let mut expanded = Vec::new();
    for ((agent, pos), event) in old {
        let mut rows = vec![event.clone()];
        match &event {
            AgentEvent::Entry(Entry::Step { at, prose, .. })
                if spoken.contains_key(&(agent, pos)) =>
            {
                if !prose.is_empty() {
                    rows.push(AgentEvent::Entry(Entry::Sent {
                        at: *at,
                        id: MessageId::new(),
                        to: Party::Human,
                        text: prose.clone(),
                    }));
                }
                if let Some(usage) = &spoken[&(agent, pos)] {
                    rows.push(AgentEvent::Entry(Entry::Usage {
                        at: *at,
                        usage: crate::entry::ResponseUsage {
                            model: usage.model.name().to_owned(),
                            input_tokens: usage.input_tokens,
                            cache_read_tokens: usage.cache_read_tokens,
                            cache_write_tokens: usage.cache_write_tokens,
                            cache_write_1h_tokens: usage.cache_write_1h_tokens,
                            output_tokens: usage.output_tokens,
                        },
                    }));
                }
            }
            AgentEvent::Transcript {
                line: TranscriptLine::Assistant { text, .. },
                at,
                ..
            } if !text.is_empty() => {
                rows.push(AgentEvent::Entry(Entry::Sent {
                    at: *at,
                    id: MessageId::new(),
                    to: Party::Human,
                    text: text.clone(),
                }));
            }
            AgentEvent::Transcript {
                line: TranscriptLine::User { text },
                at,
                wake: None,
                ..
            } if !text
                .trim_start()
                .starts_with("This session is being continued from a previous conversation") =>
            {
                let id = MessageId::new();
                rows.push(AgentEvent::Entry(Entry::Received {
                    at: *at,
                    id,
                    from: Party::Human,
                    body: vec![Block::Text(text.clone())],
                }));
                rows.push(AgentEvent::Entry(Entry::Woken {
                    at: *at,
                    why: Wake::Message,
                    report: String::new(),
                    images: Vec::new(),
                    messages: vec![id],
                    acknowledged: Vec::new(),
                    results: Vec::new(),
                }));
            }
            _ => {}
        }
        let cursor = next.entry(agent).or_default();
        let assigned = (*cursor..*cursor + rows.len() as u64).collect::<Vec<_>>();
        *cursor += rows.len() as u64;
        positions.insert((agent, pos), assigned.clone());
        expanded.extend(
            assigned
                .into_iter()
                .zip(rows)
                .map(|(pos, event)| (agent, pos, event)),
        );
    }
    // A target denotes the beginning of the old row, including anything that
    // row expanded into. Hidden branches remain hidden and readable.
    for (agent, _, event) in &mut expanded {
        if let AgentEvent::Rewound { to, .. } = event {
            *to = AgentEventPos::new(positions[&(*agent, to.pos)][0]);
        }
    }
    // Use the typed table's actual name, rather than depending on SQL/raw bytes.
    let old_keys = write
        .open_table(AGENT_LOG)
        .iter()
        .map(|(key, _)| key.value())
        .collect::<Vec<_>>();
    {
        let mut log = write.open_table(AGENT_LOG);
        for key in old_keys {
            log.remove(&key);
        }
        for (agent, pos, event) in &expanded {
            log.insert(&(*agent, *pos), SenValue::borrowed(event));
        }
    }
    let old_seqs = write
        .open_table(JOURNAL)
        .iter()
        .map(|(key, _)| key.value())
        .collect::<Vec<_>>();
    {
        let mut table = write.open_table(JOURNAL);
        for seq in old_seqs {
            table.remove(&seq);
        }
        let mut seq = 1;
        for key in journal {
            for pos in &positions[&key] {
                table.insert(&seq, &(key.0, *pos));
                seq += 1;
            }
        }
    }
    for group in expanded.chunk_by(|a, b| a.0 == b.0) {
        let agent = group[0].0;
        let rows = group
            .iter()
            .map(|(_, pos, event)| (AgentEventPos::new(*pos), event.clone()));
        let head = fold_head(rows).expect("creation remains first");
        write
            .open_table(AGENT_HEADS)
            .insert(&agent, SenValue::borrowed(&head));
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::UnixMs;

    use super::*;
    use crate::db::{AgentProfileWriteTxnExt, AgentReadTxnExt, AgentWriteTxnExt};

    #[tokio::test]
    async fn messages_expand_without_losing_rewinds_or_journal_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("migration.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let first = write.alloc_agent_id();
        let second = write.alloc_agent_id();
        for id in [first, second] {
            write.create_agent(
                UnixMs(1),
                id,
                None,
                super::super::tests::test_workspace(),
                Default::default(),
                crate::db::SessionBinding::ResponsesSol(Default::default()),
                super::super::tests::test_agent_runtime(),
                super::super::AgentOrigin::User,
            );
        }
        let row = |text: &str| AgentEvent::Transcript {
            uuid: uuid::Uuid::new_v4(),
            line: TranscriptLine::User { text: text.into() },
            at: UnixMs(3),
            wake: None,
        };
        write.append_agent_event(first, &row("first input")); // old pos 1 -> new 1,2,3
        write.append_agent_event(second, &row("other agent")); // independent positions
        write.append_agent_event(
            first,
            &AgentEvent::Transcript {
                uuid: uuid::Uuid::new_v4(),
                line: TranscriptLine::Assistant {
                    text: "historical answer".into(),
                    calls: vec![],
                    usage: None,
                    context_used: None,
                },
                at: UnixMs(5),
                wake: None,
            },
        ); // old 2 -> new 4,5
        write.append_agent_event(
            first,
            &AgentEvent::Rewound {
                to: AgentEventPos::new(2),
                at: UnixMs(6),
            },
        ); // old 3 -> new 6; hide both answer records
        write.append_agent_event(first, &row("replacement")); // old4 -> new7,8,9
        migrate(&mut write);
        let log = {
            let table = write.open_table(AGENT_LOG);
            super::super::rows(table.range(super::super::agent_range(first))).collect::<Vec<_>>()
        };
        assert_eq!(log.len(), 10);
        assert!(matches!(log[6].1, AgentEvent::Rewound { to, .. } if to.pos == 4));
        assert!(
            matches!(&log[5].1, AgentEvent::Entry(Entry::Sent { text, .. }) if text == "historical answer")
        );
        let (_, visible) = super::super::visible_rows(log.into_iter());
        assert!(
            !visible
                .iter()
                .any(|(_, event)| matches!(event, AgentEvent::Entry(Entry::Sent { .. })))
        );
        let ids = visible
            .iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Entry(Entry::Received { id, .. }) => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let delivered = visible
            .iter()
            .flat_map(|(_, event)| match event {
                AgentEvent::Entry(Entry::Woken { messages, .. }) => messages.clone(),
                _ => vec![],
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, delivered);
        let journal = write
            .open_table(JOURNAL)
            .iter()
            .map(|(_, row)| row.value())
            .collect::<Vec<_>>();
        assert_eq!(
            &journal[..8],
            &[
                (first, 0),
                (second, 0),
                (first, 1),
                (first, 2),
                (first, 3),
                (second, 1),
                (second, 2),
                (second, 3)
            ]
        );
        assert_eq!(journal.len(), 14);
        write.commit();
        assert_eq!(db.read().get_agent(first).next.pos, 10);
    }
    #[tokio::test]
    async fn native_prose_and_cost_are_migrated_but_code_first_prose_is_not_sent() {
        use rho_agent_types::ContentPart;
        use rho_inference::types::{ContextBlock, InferenceResponseItem};
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("native.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = write.alloc_agent_id();
        write.create_agent(
            UnixMs(1),
            agent,
            None,
            super::super::tests::test_workspace(),
            Default::default(),
            crate::db::SessionBinding::ResponsesSol(Default::default()),
            super::super::tests::test_agent_runtime(),
            super::super::AgentOrigin::User,
        );
        write.append_agent_event(
            agent,
            &AgentEvent::Native(super::super::legacy::NativeEvent::ResponseFinished {
                output: vec![ContextBlock::InferenceResponse {
                    items: vec![InferenceResponseItem::AssistantMessage {
                        provider_specific: Box::new(
                            rho_inference::OpenAiResponsesProviderData::Message {
                                item_id: "old-message".try_into().unwrap(),
                            },
                        ),
                        content: vec![ContentPart::Text {
                            text: "old answer".into(),
                        }],
                        phase: None,
                    }],
                    provider_response_id: None,
                }],
                context_used: Some(157),
                usage: Some(super::super::AgentUsageBucket {
                    model: super::super::AgentUsageModel::ASTRA,
                    input_tokens: 101,
                    cache_read_tokens: 37,
                    output_tokens: 19,
                    requests: 1,
                    ..Default::default()
                }),
                at: UnixMs(2),
            }),
        );
        write.append_agent_event(
            agent,
            &AgentEvent::Entry(Entry::Step {
                at: UnixMs(3),
                calls: vec![],
                prose: "private new prose".into(),
                carry: rho_inference::step::Carry::from_openai_items(vec![]),
                usage: Default::default(),
            }),
        );
        migrate(&mut write);
        write.commit();
        let (_, rows) = db.read().agent_event_records(agent);
        assert!(
            !rows
                .iter()
                .any(|(_, event)| matches!(event, AgentEvent::Native(_)))
        );
        let sent = rows
            .iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Entry(Entry::Sent { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(sent, ["old answer"]);
        assert!(rows.iter().any(|(_, event)| matches!(event,
            AgentEvent::Entry(Entry::Usage { usage, .. }) if usage.input_tokens == 101
                && usage.cache_read_tokens == 37 && usage.output_tokens == 19)));
        assert!(rows.iter().any(|(_, event)| matches!(event,
            AgentEvent::Entry(Entry::Step { usage, prose, .. }) if prose == "old answer"
                && usage.input_tokens == 138)));
    }
}
