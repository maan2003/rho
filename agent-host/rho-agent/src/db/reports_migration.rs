//! Temporary 7f24a9d3 -> dc371fa2 migration to typed reports and native
//! indexes. Drop this after active databases have opened the migrated build.
//! All branch rows are converted, including rows hidden by a later rewind.

use std::collections::{BTreeMap, BTreeSet};

use rho_agent_types::AgentId;
use rho_db::{SenValue, WriteTxn};

use super::legacy::provider;
use super::{AGENT_HEADS, AGENT_LOG, AgentEventPos, JOURNAL, legacy};
use crate::AgentEvent;
use crate::entry::{Entry, Report, ResponseUsage};
use crate::inference::Usage;

pub(super) fn migrate(write: &mut WriteTxn) {
    // This migration remaps log positions. A derived cursor, if present in
    // a partially upgraded/test store, must be rebuilt from the rewritten log.
    write.delete_table("agent_native_cursors");
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
    let heads = write
        .open_table(AGENT_HEADS)
        .iter()
        .map(|(id, head)| (id.value(), head.value().into_owned()))
        .collect::<Vec<_>>();

    // Old Usage immediately followed its Step, and was already counted in the
    // aggregate tables. Attach only to that Step; do not bill it again.
    let mut billed = BTreeMap::new();
    for pair in old.windows(2) {
        if let [
            ((agent, pos), AgentEvent::LegacyEntry(legacy::Entry::Step { .. })),
            ((next_agent, _), AgentEvent::LegacyEntry(legacy::Entry::Usage { usage, .. })),
        ] = pair
            && agent == next_agent
        {
            billed.insert((*agent, *pos), usage.clone());
        }
    }

    // Match an actual compacting attempt, not merely a queued intent. When a
    // trigger follows a Woken, the send belongs at the trigger: a rewind can
    // keep the Woken while hiding that later trigger and response.
    let mut compact_woken = BTreeSet::new();
    let mut synth_trigger = BTreeSet::new();
    let mut visible = Vec::<(u64, Option<u64>, bool)>::new();
    let mut current_agent = None;
    let mut trigger = None;
    let mut sent = false;
    for ((agent, pos), event) in &old {
        if current_agent != Some(*agent) {
            current_agent = Some(*agent);
            visible.clear();
            (trigger, sent) = (None, false);
        }
        if let AgentEvent::Rewound { to, .. } = event {
            visible.retain(|(previous, _, _)| *previous < to.pos);
            (trigger, sent) = visible.last().map_or((None, false), |(_, t, s)| (*t, *s));
        }
        match event {
            AgentEvent::LegacyEntry(legacy::Entry::CompactionTrigger { .. }) => {
                trigger = Some(*pos);
            }
            AgentEvent::LegacyEntry(legacy::Entry::Woken { .. }) => {
                if trigger.is_some() {
                    compact_woken.insert((*agent, *pos));
                    sent = true;
                }
            }
            AgentEvent::LegacyEntry(legacy::Entry::Step { carry, .. }) => {
                if carry.has_compaction() && trigger.is_some() && !sent {
                    synth_trigger.insert((*agent, trigger.expect("compaction trigger")));
                    sent = true;
                }
                if sent || carry.has_compaction() {
                    trigger = None;
                    sent = false;
                }
            }
            _ => {}
        }
        visible.push((*pos, trigger, sent));
    }

    let mut next = BTreeMap::<AgentId, u64>::new();
    // A removed row maps to the first surviving row after it. This preserves
    // rewind targets even when a branch begins on a removed Usage/Activity.
    let mut positions = BTreeMap::<(AgentId, u64), u64>::new();
    let mut expanded = Vec::new();
    let mut retained = BTreeSet::new();
    for ((agent, pos), event) in &old {
        let converted = match event {
            AgentEvent::LegacyEntry(entry) => match entry {
                legacy::Entry::Step {
                    at,
                    calls,
                    prose,
                    carry,
                    usage,
                } => {
                    let usage = billed.get(&(*agent, *pos)).cloned().or_else(|| {
                        (*usage != Usage::default())
                            .then(|| ResponseUsage::rho("unknown".into(), *usage))
                    });
                    Some(AgentEvent::Entry(Entry::Step {
                        at: *at,
                        exec: calls.first().map(|call| call.code.clone()),
                        prose: prose.clone(),
                        carry: carry.into_live(calls),
                        usage,
                    }))
                }
                legacy::Entry::Usage { .. } | legacy::Entry::Activity { .. } => None,
                legacy::Entry::Woken {
                    at,
                    why,
                    report,
                    images,
                    messages,
                    acknowledged,
                    results,
                } => Some(AgentEvent::Entry(Entry::RequestSent {
                    at: *at,
                    why: *why,
                    report: Report {
                        notebook: rho_notebook::Report::from_text(
                            report.clone(),
                            images
                                .iter()
                                .map(|image| rho_notebook::Image {
                                    media_type: image.media_type.clone(),
                                    data: image.data.clone(),
                                })
                                .collect(),
                        ),
                        messages: messages.clone(),
                        acknowledged: acknowledged.clone(),
                        ..Default::default()
                    },
                    compact: compact_woken.contains(&(*agent, *pos)),
                    imported: Some(provider::imported(report, images, results)),
                })),
                legacy::Entry::CompactionTrigger { at, manual } => {
                    Some(AgentEvent::Entry(Entry::CompactionTrigger {
                        at: *at,
                        manual: *manual,
                    }))
                }
                legacy::Entry::Received { at, id, from, body } => {
                    Some(AgentEvent::Entry(Entry::Received {
                        at: *at,
                        id: *id,
                        from: *from,
                        body: body.clone(),
                    }))
                }
                legacy::Entry::Sent { at, id, to, text } => Some(AgentEvent::Entry(Entry::Sent {
                    at: *at,
                    id: *id,
                    to: *to,
                    text: text.clone(),
                })),
                legacy::Entry::Status { at, text } => Some(AgentEvent::Entry(Entry::Status {
                    at: *at,
                    text: text.clone(),
                })),
                legacy::Entry::Awaiting { at, since } => Some(AgentEvent::Entry(Entry::Awaiting {
                    at: *at,
                    since: *since,
                })),
                legacy::Entry::Notice { at, notice } => Some(AgentEvent::Entry(Entry::Notice {
                    at: *at,
                    notice: notice.clone(),
                })),
            },
            other => Some(other.clone()),
        };
        let cursor = next.entry(*agent).or_default();
        positions.insert((*agent, *pos), *cursor);
        if let Some(converted) = converted {
            retained.insert((*agent, *pos));
            expanded.push((*agent, *cursor, converted));
            *cursor += 1;
            if synth_trigger.contains(&(*agent, *pos)) {
                let at = match event {
                    AgentEvent::LegacyEntry(legacy::Entry::CompactionTrigger { at, .. }) => *at,
                    _ => unreachable!("only triggers synthesize sends"),
                };
                expanded.push((
                    *agent,
                    *cursor,
                    AgentEvent::Entry(Entry::RequestSent {
                        at,
                        why: crate::entry::Wake::Compaction,
                        report: Report::default(),
                        compact: true,
                        // This historical trigger consumed no messages.
                        imported: Some(provider::imported("", &[], &[])),
                    }),
                ));
                *cursor += 1;
            }
        }
    }
    for (agent, _, event) in &mut expanded {
        if let AgentEvent::Rewound { to, .. } = event {
            *to = AgentEventPos::new(positions[&(*agent, to.pos)]);
        }
    }
    {
        let mut log = write.open_table(AGENT_LOG);
        for ((agent, pos), _) in &old {
            log.remove(&(*agent, *pos));
        }
        for (agent, pos, event) in &expanded {
            log.insert(&(*agent, *pos), SenValue::borrowed(event));
        }
    }
    {
        let mut table = write.open_table(JOURNAL);
        for (seq, _) in table
            .iter()
            .map(|(key, value)| (key.value(), value.value()))
            .collect::<Vec<_>>()
        {
            table.remove(&seq);
        }
        let mut seq = 1;
        for key in journal {
            if retained.contains(&key) {
                table.insert(&seq, &(key.0, positions[&key]));
                seq += 1;
                if synth_trigger.contains(&key) {
                    table.insert(&seq, &(key.0, positions[&key] + 1));
                    seq += 1;
                }
            }
        }
    }
    for (agent, mut head) in heads {
        head.next = AgentEventPos::new(next[&agent]);
        write
            .open_table(AGENT_HEADS)
            .insert(&agent, SenValue::borrowed(&head));
        // Build the index once, after all log positions and rewinds are final.
        let cursor = super::native::rebuild(write, agent);
        write
            .open_table(super::native::NATIVE_CURSORS)
            .insert(&agent, SenValue::borrowed(&cursor));
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{Seq, UnixMs};
    use rho_db::RhoDb;

    use super::legacy::provider::{Call, CallResult, Carry};
    use super::*;
    use crate::db::{
        AgentOrigin, AgentProfileWriteTxnExt, AgentReadTxnExt, AgentWriteTxnExt, FORMAT,
        SessionBinding,
    };
    use crate::entry::{MessageId, Party, Wake};
    use crate::inference::Image;

    mod old_wire {
        #[derive(senax_encoder::Encode)]
        pub enum AgentEvent {
            Entry(super::legacy::Entry),
        }
    }

    #[test]
    fn original_entry_wire_tag_decodes_as_legacy_entry() {
        let original = legacy::Entry::Woken {
            at: UnixMs(47),
            why: Wake::Returned,
            report: "old response".into(),
            images: vec![Image {
                media_type: "image/png".into(),
                data: vec![4, 8],
            }],
            messages: vec![MessageId(8)],
            acknowledged: vec![MessageId(9)],
            results: vec![CallResult::from_legacy("old-call", "answer".into(), vec![])],
        };
        let mut bytes =
            senax_encoder::encode(&old_wire::AgentEvent::Entry(original.clone())).unwrap();
        let decoded: AgentEvent<'static> = senax_encoder::decode(&mut bytes).unwrap();
        assert_eq!(decoded, AgentEvent::LegacyEntry(original));
    }

    #[tokio::test]
    async fn rewind_target_on_removed_usage_points_to_next_surviving_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("removed-target.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let id = write.alloc_agent_id();
        write.create_agent(
            UnixMs(1),
            id,
            None,
            super::super::tests::test_workspace(),
            Default::default(),
            SessionBinding::ResponsesSol(Default::default()),
            super::super::tests::test_agent_runtime(),
            AgentOrigin::User,
        );
        let add = |write: &mut WriteTxn, entry| {
            write.append_agent_event(id, &AgentEvent::LegacyEntry(entry));
        };
        add(
            &mut write,
            legacy::Entry::Step {
                at: UnixMs(2),
                calls: vec![],
                prose: "keep".into(),
                carry: Carry::from_openai_items(vec![]),
                usage: Usage::default(),
            },
        ); // old 1 -> new 1
        add(
            &mut write,
            legacy::Entry::Usage {
                at: UnixMs(2),
                usage: ResponseUsage {
                    model: "sol".into(),
                    input_tokens: 2,
                    cache_read_tokens: 1,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    output_tokens: 3,
                },
            },
        ); // old 2 removed -> new 2 (the next surviving row)
        add(
            &mut write,
            legacy::Entry::Status {
                at: UnixMs(3),
                text: "hidden".into(),
            },
        ); // old 3 -> new 2
        write.append_agent_event(
            id,
            &AgentEvent::Rewound {
                to: AgentEventPos::new(2),
                at: UnixMs(4),
            },
        ); // old 4 -> new 3
        // A derived cursor from the pre-rewrite positions must be discarded.
        let mut stale = super::super::native::NativeCursor::default();
        stale.from = AgentEventPos::new(3);
        write
            .open_table(super::super::native::NATIVE_CURSORS)
            .insert(&id, SenValue::borrowed(&stale));
        write.open_table(FORMAT).insert(&(), &"7f24a9d3".to_owned());
        write.commit();

        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        let read = db.read();
        let cursor = read
            .open_table(super::super::native::NATIVE_CURSORS)
            .get(&id)
            .expect("migration must build the index")
            .value()
            .into_owned();
        assert_eq!(cursor.from, AgentEventPos::ZERO);
        assert_eq!(cursor.recovery.compaction.context_used, Some(6));
        assert_eq!(read.agent_context_boundary(id).from, AgentEventPos::ZERO);
        assert_eq!(
            read.agent_context_boundary(id).through,
            AgentEventPos::new(4)
        );
        assert!(
            matches!(read.agent_event(id, AgentEventPos::new(3)), Some(AgentEvent::Rewound { to, .. }) if to.pos == 2)
        );
        let (_, visible) = read.agent_events(id);
        assert_eq!(visible.len(), 3); // Created, Step, Rewound; Status was hidden.
        assert!(
            matches!(&visible[1], AgentEvent::Entry(Entry::Step { prose, .. }) if prose == "keep")
        );
    }

    #[tokio::test]
    async fn compaction_marks_only_sent_attempts_and_manual_only_send() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("compaction.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let ids = (0..4)
            .map(|_| {
                let id = write.alloc_agent_id();
                write.create_agent(
                    UnixMs(1),
                    id,
                    None,
                    super::super::tests::test_workspace(),
                    Default::default(),
                    SessionBinding::ResponsesSol(Default::default()),
                    super::super::tests::test_agent_runtime(),
                    AgentOrigin::User,
                );
                id
            })
            .collect::<Vec<_>>();
        let woken = |text: &str| legacy::Entry::Woken {
            at: UnixMs(2),
            why: Wake::Message,
            report: text.into(),
            images: vec![],
            messages: vec![],
            acknowledged: vec![],
            results: vec![],
        };
        let step = |compacted| legacy::Entry::Step {
            at: UnixMs(3),
            calls: vec![],
            prose: String::new(),
            carry: Carry::from_openai_items(if compacted {
                vec![r#"{"type":"compaction","encrypted_content":"summary"}"#.into()]
            } else {
                vec![]
            }),
            usage: Usage::default(),
        };
        let append = |write: &mut WriteTxn, id, entry| {
            write.append_agent_event(id, &AgentEvent::LegacyEntry(entry));
        };
        // Auto compaction: the trigger comes after the report it accompanied.
        append(&mut write, ids[0], woken("auto"));
        append(
            &mut write,
            ids[0],
            legacy::Entry::CompactionTrigger {
                at: UnixMs(2),
                manual: false,
            },
        );
        append(&mut write, ids[0], step(true));
        append(&mut write, ids[0], woken("next"));
        // Manual-only compaction: no Woken exists to carry the provider send.
        append(
            &mut write,
            ids[1],
            legacy::Entry::CompactionTrigger {
                at: UnixMs(2),
                manual: true,
            },
        );
        append(&mut write, ids[1], step(true));
        append(&mut write, ids[1], woken("after manual"));
        // A manual trigger queued after an in-flight request did not enter it.
        append(&mut write, ids[2], woken("in flight"));
        append(
            &mut write,
            ids[2],
            legacy::Entry::CompactionTrigger {
                at: UnixMs(2),
                manual: true,
            },
        );
        append(&mut write, ids[2], step(false));
        append(&mut write, ids[2], woken("queued intent now sent"));
        append(&mut write, ids[2], step(true));
        // Rewinding to the trigger hides both the trigger and compacted reply,
        // but leaves the preceding Woken on the visible branch.
        append(&mut write, ids[3], woken("survives rewind")); // old 1
        append(
            &mut write,
            ids[3],
            legacy::Entry::CompactionTrigger {
                at: UnixMs(2),
                manual: false,
            },
        ); // old 2
        append(&mut write, ids[3], step(true)); // old 3
        write.append_agent_event(
            ids[3],
            &AgentEvent::Rewound {
                to: AgentEventPos::new(2),
                at: UnixMs(4),
            },
        ); // old 4
        write.open_table(FORMAT).insert(&(), &"7f24a9d3".to_owned());
        write.commit();

        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        let read = db.read();
        let entries = |id| {
            read.agent_events(id)
                .1
                .into_iter()
                .filter_map(|event| match event {
                    AgentEvent::Entry(entry) => Some(entry),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let auto = entries(ids[0]);
        assert!(matches!(
            &auto[0],
            Entry::RequestSent { compact: false, .. }
        ));
        assert!(
            matches!(&auto[2], Entry::RequestSent { compact: true, imported: Some(_), report, .. } if report.is_empty())
        );
        assert!(matches!(
            &auto[4],
            Entry::RequestSent { compact: false, .. }
        ));
        let manual = entries(ids[1]);
        assert!(matches!(
            &manual[0],
            Entry::CompactionTrigger { manual: true, .. }
        ));
        assert!(
            matches!(&manual[1], Entry::RequestSent { compact: true, imported: Some(_), report, .. } if report.is_empty())
        );
        assert!(matches!(
            &manual[3],
            Entry::RequestSent { compact: false, .. }
        ));
        let queued = entries(ids[2]);
        assert!(matches!(
            &queued[0],
            Entry::RequestSent { compact: false, .. }
        ));
        assert!(matches!(
            &queued[3],
            Entry::RequestSent { compact: true, .. }
        ));
        let surviving = entries(ids[3]);
        assert_eq!(surviving.len(), 1);
        assert!(matches!(
            &surviving[0],
            Entry::RequestSent { compact: false, .. }
        ));
        let visible_request = crate::agent::context::request(
            "".into(),
            &surviving,
            crate::inference::CacheKey::from_u128(0),
        );
        assert!(
            !visible_request
                .items()
                .iter()
                .any(|item| matches!(item, crate::inference::Item::CompactionTrigger))
        );
        assert!(visible_request.items().iter().any(
            |item| matches!(item, crate::inference::Item::Step { carry, .. }
                    if serde_json::from_str::<serde_json::Value>(carry.data().get()).unwrap()["imported"]["text"] == "survives rewind"
                        && !carry.has_compaction())
        ));
        assert!(
            matches!(read.agent_event(ids[3], AgentEventPos::new(5)), Some(AgentEvent::Rewound { to, .. }) if to.pos == 2)
        );
        let request = crate::agent::context::request(
            "".into(),
            &manual,
            crate::inference::CacheKey::from_u128(0),
        );
        assert!(
            request
                .items()
                .iter()
                .any(|item| matches!(item, crate::inference::Item::Step { carry, .. } if carry.has_compaction()))
        );
        drop(read);
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        let read = db.read();
        assert_eq!(
            read.agent_events(ids[1])
                .1
                .into_iter()
                .filter(|event| matches!(
                    event,
                    AgentEvent::Entry(Entry::RequestSent {
                        compact: true,
                        imported: Some(_),
                        ..
                    })
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn migrates_legacy_wire_reports_branches_usage_and_journal_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("agent.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = write.alloc_agent_id();
        let other = write.alloc_agent_id();
        for id in [agent, other] {
            write.create_agent(
                UnixMs(1),
                id,
                None,
                super::super::tests::test_workspace(),
                Default::default(),
                SessionBinding::ResponsesSol(Default::default()),
                super::super::tests::test_agent_runtime(),
                AgentOrigin::User,
            );
        }
        let add = |write: &mut WriteTxn, event: legacy::Entry| {
            write.append_agent_event(agent, &AgentEvent::LegacyEntry(event));
        };
        add(
            &mut write,
            legacy::Entry::Received {
                at: UnixMs(2),
                id: MessageId(11),
                from: Party::Human,
                body: vec![crate::entry::Block::Text("chosen".into())],
            },
        ); // 1
        add(
            &mut write,
            legacy::Entry::Received {
                at: UnixMs(3),
                id: MessageId(12),
                from: Party::Human,
                body: vec![crate::entry::Block::Text("acknowledged".into())],
            },
        ); // 2
        let response = Carry::from_openai_items(vec![
            r#"{"type":"custom_tool_call","id":"ctc_first","call_id":"first","name":"exec","input":"print(1)"}"#.into(),
        ]);
        add(
            &mut write,
            legacy::Entry::Step {
                at: UnixMs(4),
                calls: vec![
                    Call::from_legacy("first", "print(1)".into()),
                    Call::from_legacy("evicted", "print(2)".into()),
                ],
                prose: "private".into(),
                carry: response.clone(),
                usage: Usage {
                    input_tokens: 11,
                    cached_tokens: 3,
                    output_tokens: 5,
                },
            },
        ); // 3
        // Original encoded AgentEvent::Entry tag is decoded through LegacyEntry.
        let original = write
            .open_table(AGENT_LOG)
            .get(&(agent, 3))
            .unwrap()
            .value()
            .into_owned();
        assert!(matches!(
            original,
            AgentEvent::LegacyEntry(legacy::Entry::Step { .. })
        ));
        let billed = ResponseUsage {
            model: "gpt-6-sol".into(),
            input_tokens: 17,
            cache_read_tokens: 7,
            cache_write_tokens: 2,
            cache_write_1h_tokens: 1,
            output_tokens: 9,
        };
        add(
            &mut write,
            legacy::Entry::Usage {
                at: UnixMs(4),
                usage: billed.clone(),
            },
        ); // 4 removed
        write.append_agent_event(
            other,
            &AgentEvent::LegacyEntry(legacy::Entry::Status {
                at: UnixMs(5),
                text: "other".into(),
            }),
        );
        let images = vec![Image {
            media_type: "image/png".into(),
            data: vec![7, 19],
        }];
        let results = vec![
            CallResult::from_legacy("first", "alpha".into(), vec![]),
            CallResult::from_legacy("evicted", "beta".into(), images.clone()),
        ];
        add(
            &mut write,
            legacy::Entry::Woken {
                at: UnixMs(6),
                why: Wake::Message,
                report: "literal text".into(),
                images: images.clone(),
                messages: vec![MessageId(11)],
                acknowledged: vec![MessageId(12)],
                results: results.clone(),
            },
        ); // 5 -> 4
        add(
            &mut write,
            legacy::Entry::Activity {
                at: UnixMs(7),
                responding: true,
                running_tasks: 3,
                checkin_at: Some(UnixMs(8)),
                archived: false,
            },
        ); // 6 removed
        add(
            &mut write,
            legacy::Entry::CompactionTrigger {
                at: UnixMs(8),
                manual: true,
            },
        ); // 7 -> 5
        add(
            &mut write,
            legacy::Entry::Woken {
                at: UnixMs(9),
                why: Wake::Compaction,
                report: "first branch".into(),
                images: vec![],
                messages: vec![],
                acknowledged: vec![],
                results: vec![],
            },
        ); // 8 -> 6, hidden
        add(
            &mut write,
            legacy::Entry::Step {
                at: UnixMs(10),
                calls: vec![],
                prose: String::new(),
                carry: Carry::from_openai_items(vec![
                    r#"{"type":"compaction","encrypted_content":"x"}"#.into(),
                ]),
                usage: Usage::default(),
            },
        ); // 9 -> 7, hidden; clears compaction on this branch
        write.append_agent_event(
            agent,
            &AgentEvent::Rewound {
                to: AgentEventPos::new(8),
                at: UnixMs(11),
            },
        ); // 10 -> 8, hidden branch starts at 6
        add(
            &mut write,
            legacy::Entry::Woken {
                at: UnixMs(12),
                why: Wake::Compaction,
                report: "after rewind".into(),
                images: vec![],
                messages: vec![],
                acknowledged: vec![],
                results: vec![],
            },
        ); // 11 -> 9, compaction trigger from pos 7 is visible again
        add(
            &mut write,
            legacy::Entry::Step {
                at: UnixMs(13),
                calls: vec![],
                prose: "unbilled".into(),
                carry: Carry::from_openai_items(vec![]),
                usage: Usage {
                    input_tokens: 23,
                    cached_tokens: 4,
                    output_tokens: 6,
                },
            },
        ); // 12 -> 10, unknown model fallback
        write.add_agent_usage(
            agent,
            &super::super::AgentUsageBucket {
                bucket_start_ms: super::super::AGENT_USAGE_BUCKET_MS,
                model: super::super::AgentUsageModel::GPT,
                input_tokens: 101,
                output_tokens: 23,
                requests: 1,
                ..Default::default()
            },
        );
        let previous_head = write
            .open_table(AGENT_HEADS)
            .get(&agent)
            .unwrap()
            .value()
            .into_owned();
        write.open_table(FORMAT).insert(&(), &"7f24a9d3".to_owned());
        write.commit();

        let mut write = db.write().await;
        write.init_agent_tables();
        let journal = write
            .open_table(JOURNAL)
            .iter()
            .map(|(seq, row)| (Seq(seq.value()), row.value()))
            .collect::<Vec<_>>();
        let all = journal
            .into_iter()
            .map(|(seq, (id, pos))| {
                (
                    seq,
                    id,
                    AgentEventPos::new(pos),
                    write
                        .open_table(AGENT_LOG)
                        .get(&(id, pos))
                        .unwrap()
                        .value()
                        .into_owned(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            all.iter()
                .enumerate()
                .all(|(i, (seq, _, _, _))| seq.0 == i as u64 + 1)
        );
        assert!(
            all.iter()
                .all(|(_, _, _, event)| !matches!(event, AgentEvent::LegacyEntry(_)))
        );
        assert_eq!(all.len(), 13); // Two created rows plus 11 retained entries.
        assert_eq!(
            all.iter()
                .map(|(_, id, pos, _)| (*id, pos.pos))
                .collect::<Vec<_>>(),
            vec![
                (agent, 0),
                (other, 0),
                (agent, 1),
                (agent, 2),
                (agent, 3),
                (other, 1),
                (agent, 4),
                (agent, 5),
                (agent, 6),
                (agent, 7),
                (agent, 8),
                (agent, 9),
                (agent, 10),
            ]
        );
        let actual_head = write
            .open_table(AGENT_HEADS)
            .get(&agent)
            .unwrap()
            .value()
            .into_owned();
        assert_eq!(actual_head.next.pos, 11);
        let mut expected_head = previous_head;
        expected_head.next = AgentEventPos::new(11);
        assert_eq!(actual_head, expected_head);
        let rows = (0..11)
            .map(|pos| {
                write
                    .open_table(AGENT_LOG)
                    .get(&(agent, pos))
                    .unwrap()
                    .value()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        let AgentEvent::Entry(Entry::Step {
            exec, carry, usage, ..
        }) = &rows[3]
        else {
            panic!("first step")
        };
        assert_eq!(exec.as_deref(), Some("print(1)"));
        assert_eq!(usage.as_ref(), Some(&billed));
        assert_eq!(
            carry
                .display_calls()
                .iter()
                .map(|call| call.code.as_str())
                .collect::<Vec<_>>(),
            vec!["print(1)", "print(2)"]
        );
        let AgentEvent::Entry(Entry::RequestSent {
            report,
            imported: Some(imported),
            compact,
            ..
        }) = &rows[4]
        else {
            panic!("typed report")
        };
        assert!(!compact);
        assert_eq!(report.messages, vec![MessageId(11)]);
        assert_eq!(report.acknowledged, vec![MessageId(12)]);
        assert_eq!(report.notebook.render().text, "literal text");
        assert_eq!(report.notebook.render().images[0].data, images[0].data);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(imported.data().get()).unwrap(),
            serde_json::json!({"imported": {
                "text": "literal text", "images": [{"media_type": "image/png", "data": [7, 19]}],
                "results": [
                    {"id": "first", "text": "alpha", "images": []},
                    {"id": "evicted", "text": "beta", "images": [{"media_type": "image/png", "data": [7, 19]}]}
                ]
            }})
        );
        let migrated = rows[1..=4]
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Entry(entry) => Some(entry.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let projected = crate::agent::context::request(
            "".into(),
            &migrated,
            crate::inference::CacheKey::from_u128(0),
        );
        assert!(projected.items().iter().any(|item| matches!(item,
            crate::inference::Item::Step { carry, .. } if serde_json::from_str::<serde_json::Value>(carry.data().get()).unwrap()["items"][0]["call_id"] == "first"
        )));
        assert!(projected.items().iter().any(|item| matches!(item,
            crate::inference::Item::Step { carry, .. } if serde_json::from_str::<serde_json::Value>(carry.data().get()).unwrap()["imported"]["results"][1]["id"] == "evicted"
        )));
        assert!(projected.items().iter().any(|item| matches!(item,
            crate::inference::Item::User { text, .. } if text.contains("chosen")
        )));
        assert!(!projected.items().iter().any(|item| matches!(item,
            crate::inference::Item::User { text, .. } if text.contains("acknowledged")
        )));
        assert!(matches!(
            &rows[6],
            AgentEvent::Entry(Entry::RequestSent { compact: true, .. })
        ));
        assert!(matches!(&rows[8], AgentEvent::Rewound { to, .. } if to.pos == 6));
        assert!(matches!(
            &rows[9],
            AgentEvent::Entry(Entry::RequestSent { compact: true, .. })
        ));
        assert!(
            matches!(&rows[10], AgentEvent::Entry(Entry::Step { usage: Some(ResponseUsage { model, input_tokens: 19, cache_read_tokens: 4, output_tokens: 6, .. }), .. }) if model == "unknown")
        );
        write.commit();
        let read = db.read();
        assert_eq!(read.agent_events(agent).1.len(), 9); // Rewound hides rows 6 and 7.
        assert_eq!(read.agent_usage_total(agent).input_tokens, 101);
        assert_eq!(read.global_agent_usage(UnixMs(0))[0].1.requests, 1);
        drop(read);

        // A second open is a no-op: no journals or positions are counted twice.
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        let read = db.read();
        assert_eq!(read.journal_since(Seq(0), 100), all);
        assert_eq!(read.agent_usage_total(agent).input_tokens, 101);
    }
}
