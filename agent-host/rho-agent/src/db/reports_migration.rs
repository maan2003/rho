//! Temporary dc371fa2 migration of remaining legacy rows to typed reports and
//! native indexes. Drop this after active databases have opened the migrated
//! build. All branch rows are converted, including rows hidden by a later
//! rewind.

use std::collections::{BTreeMap, BTreeSet};

use rho_agent_types::AgentId;
use rho_db::{SenValue, WriteTxn};

use super::legacy::provider;
use super::{AGENT_HEADS, AGENT_LOG, AgentEventPos, AgentRuntime, JOURNAL, fold_head, legacy};
use crate::AgentEvent;
use crate::entry::{Entry, Report, ResponseUsage};
use crate::inference::Usage;

pub(super) fn migrate(
    write: &mut WriteTxn,
    native_spoken: &BTreeMap<(AgentId, u64), Option<super::AgentUsageBucket>>,
    native_failed: &BTreeMap<(AgentId, u64), (rho_agent_types::UnixMs, String)>,
    synthetic: &BTreeMap<(AgentId, u64), Vec<legacy::Entry>>,
) {
    // This migration remaps log positions. A derived cursor, if present in
    // a partially upgraded/test store, must be rebuilt from the rewritten log.
    write.delete_table("agent_native_cursors");
    // Only agents with old rows can gain/remove positions. Decoding all
    // physical rows is needed for discovery, but cloning/reinserting millions
    // of already-current rows would make opening a live database prohibitively
    // slow and multiply its page use during the recovery savepoint.
    let affected = write
        .open_table(AGENT_LOG)
        .iter()
        .filter_map(|(key, row)| {
            matches!(
                row.value().into_owned(),
                AgentEvent::LegacyEntry(_) | AgentEvent::Accepted(_) | AgentEvent::Cleared { .. }
            )
            .then_some(key.value().0)
        })
        .collect::<BTreeSet<_>>();
    eprintln!(
        "rho-agent history migration: {} agents have old rows to remap",
        affected.len()
    );
    let old = write
        .open_table(AGENT_LOG)
        .iter()
        .filter(|(key, _)| affected.contains(&key.value().0))
        .map(|(key, row)| (key.value(), row.value().into_owned()))
        .collect::<Vec<_>>();
    let journal = write
        .open_table(JOURNAL)
        .iter()
        .map(|(_, row)| row.value())
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
    let mut added = BTreeMap::<(AgentId, u64), Vec<u64>>::new();
    let mut expanded = Vec::new();
    let mut retained = BTreeSet::new();
    let rewind_targets = old
        .iter()
        .filter_map(|((agent, _), event)| match event {
            AgentEvent::Rewound { to, .. } => Some((*agent, to.pos)),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut before =
        BTreeMap::<(AgentId, u64), (Vec<crate::entry::MessageId>, Option<AgentRuntime>)>::new();
    let mut pending = Vec::<crate::entry::MessageId>::new();
    let mut runtime = None::<AgentRuntime>;
    let mut previous_agent = None;
    for ((agent, pos), event) in &old {
        if previous_agent != Some(*agent) {
            pending.clear();
            runtime = None;
            previous_agent = Some(*agent);
        }
        if rewind_targets.contains(&(*agent, *pos)) {
            before.insert((*agent, *pos), (pending.clone(), runtime.clone()));
        }
        let accepted_id = if matches!(
            event,
            AgentEvent::Accepted(crate::QueuedInput {
                kind: crate::InputKind::Message { .. },
                ..
            })
        ) {
            Some(crate::entry::MessageId::new())
        } else {
            None
        };
        let cleared = matches!(event, AgentEvent::Cleared { .. });
        let cleared_ids = if cleared {
            std::mem::take(&mut pending)
        } else {
            Vec::new()
        };
        let converted = match event {
            AgentEvent::LegacyEntry(entry) => match entry {
                legacy::Entry::Step {
                    at,
                    calls,
                    prose,
                    carry,
                    usage,
                } => {
                    let usage = native_spoken
                        .get(&(*agent, *pos))
                        .and_then(|bucket| {
                            bucket.as_ref().map(|bucket| ResponseUsage {
                                model: bucket.model.name().to_owned(),
                                input_tokens: bucket.input_tokens,
                                cache_read_tokens: bucket.cache_read_tokens,
                                cache_write_tokens: bucket.cache_write_tokens,
                                cache_write_1h_tokens: bucket.cache_write_1h_tokens,
                                output_tokens: bucket.output_tokens,
                            })
                        })
                        .or_else(|| billed.get(&(*agent, *pos)).cloned())
                        .or_else(|| {
                            (*usage != Usage::default())
                                .then(|| ResponseUsage::rho("unknown".into(), *usage))
                        });
                    Some(AgentEvent::Entry(Entry::Step {
                        at: *at,
                        exec: (!native_failed.contains_key(&(*agent, *pos)))
                            .then(|| calls.first().map(|call| call.code.clone()))
                            .flatten(),
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
            AgentEvent::Accepted(input) => match &input.kind {
                crate::InputKind::Message { content } => Some(AgentEvent::Entry(Entry::Received {
                    at: input.at,
                    id: accepted_id.expect("message id assigned"),
                    from: super::entries_migration::party(input.source),
                    body: content
                        .iter()
                        .map(super::entries_migration::block_of)
                        .collect(),
                })),
                crate::InputKind::Compaction => Some(AgentEvent::Entry(Entry::Sent {
                    at: input.at,
                    id: crate::entry::MessageId::new(),
                    to: crate::entry::Party::Human,
                    text: "[Historical event: compaction requested.]".into(),
                })),
            },
            AgentEvent::Cleared { at } => Some(AgentEvent::Entry(Entry::RequestSent {
                at: *at,
                why: crate::entry::Wake::Message,
                report: Report {
                    acknowledged: cleared_ids,
                    ..Default::default()
                },
                compact: false,
                // The pre-existing Claude queue was cleared; do not retry it.
                imported: Some(provider::imported("", &[], &[])),
            })),
            other => Some(other.clone()),
        };
        let cursor = next.entry(*agent).or_default();
        positions.insert((*agent, *pos), *cursor);
        let mut new_rows = Vec::new();
        if converted.is_some() {
            for extra in synthetic.get(&(*agent, *pos)).into_iter().flatten() {
                let legacy::Entry::Received { at, id, from, body } = extra else {
                    panic!("only Received can precede a historical request");
                };
                new_rows.push(*cursor);
                expanded.push((
                    *agent,
                    *cursor,
                    AgentEvent::Entry(Entry::Received {
                        at: *at,
                        id: *id,
                        from: *from,
                        body: body.clone(),
                    }),
                ));
                *cursor += 1;
            }
        } else {
            assert!(
                !synthetic.contains_key(&(*agent, *pos)),
                "synthetic message has no request"
            );
        }
        if let Some(converted) = converted {
            new_rows.push(*cursor);
            retained.insert((*agent, *pos));
            expanded.push((*agent, *cursor, converted));
            *cursor += 1;
            if let Some(id) = accepted_id {
                new_rows.push(*cursor);
                expanded.push((
                    *agent,
                    *cursor,
                    AgentEvent::Entry(Entry::RequestSent {
                        at: match event {
                            AgentEvent::Accepted(input) => input.at,
                            _ => unreachable!(),
                        },
                        why: crate::entry::Wake::Message,
                        report: Report {
                            acknowledged: vec![id],
                            ..Default::default()
                        },
                        compact: false,
                        imported: Some(provider::imported("", &[], &[])),
                    }),
                ));
                *cursor += 1;
            }
            if native_spoken.contains_key(&(*agent, *pos))
                && let AgentEvent::LegacyEntry(legacy::Entry::Step { at, prose, .. }) = event
                && !prose.is_empty()
            {
                new_rows.push(*cursor);
                expanded.push((
                    *agent,
                    *cursor,
                    AgentEvent::Entry(Entry::Sent {
                        at: *at,
                        id: crate::entry::MessageId::new(),
                        to: crate::entry::Party::Human,
                        text: prose.clone(),
                    }),
                ));
                *cursor += 1;
            }
            if let Some((at, error)) = native_failed.get(&(*agent, *pos)) {
                new_rows.push(*cursor);
                expanded.push((
                    *agent,
                    *cursor,
                    AgentEvent::Entry(Entry::Notice {
                        at: *at,
                        notice: crate::entry::Notice::Error(error.clone()),
                    }),
                ));
                *cursor += 1;
            }
            if synth_trigger.contains(&(*agent, *pos)) {
                let at = match event {
                    AgentEvent::LegacyEntry(legacy::Entry::CompactionTrigger { at, .. }) => *at,
                    _ => unreachable!("only triggers synthesize sends"),
                };
                new_rows.push(*cursor);
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
        // The historical branch's pending queue determines what Cleared
        // acknowledged, independently of later branches hidden by a rewind.
        match event {
            AgentEvent::Created {
                runtime: initial, ..
            } => runtime = Some(initial.clone()),
            AgentEvent::RuntimeRebound { change, .. } => match change {
                crate::RuntimeChange::ClaudeRewound { session_id } => {
                    runtime = Some(AgentRuntime::Claude {
                        session_id: *session_id,
                    })
                }
                crate::RuntimeChange::PromptCacheKey(key) => {
                    runtime = Some(AgentRuntime::Rho {
                        prompt_cache_key: *key,
                    })
                }
                crate::RuntimeChange::ClaudeRewindPending(_) => {}
            },
            AgentEvent::Rewound { to, .. } => {
                (pending, runtime) = before
                    .get(&(*agent, to.pos))
                    .expect("rewind target must precede row")
                    .clone();
            }
            AgentEvent::Entry(Entry::Received { id, .. })
            | AgentEvent::LegacyEntry(legacy::Entry::Received { id, .. }) => pending.push(*id),
            AgentEvent::Entry(Entry::RequestSent {
                report, imported, ..
            }) => {
                if imported.is_none() && matches!(runtime, Some(AgentRuntime::Rho { .. })) {
                    pending.clear();
                } else {
                    pending.retain(|id| {
                        !report.messages.contains(id) && !report.acknowledged.contains(id)
                    });
                }
            }
            AgentEvent::LegacyEntry(legacy::Entry::Woken {
                messages,
                acknowledged,
                ..
            }) => {
                pending.retain(|id| !messages.contains(id) && !acknowledged.contains(id));
            }
            _ => {}
        }
        added.insert((*agent, *pos), new_rows);
    }
    for (agent, _, event) in &mut expanded {
        if let AgentEvent::Rewound { to, .. } = event {
            *to = AgentEventPos::new(positions[&(*agent, to.pos)]);
        }
    }
    {
        let mut log = write.open_table(AGENT_LOG);
        for (key, _) in &old {
            if expanded
                .binary_search_by_key(key, |(agent, pos, _)| (*agent, *pos))
                .is_err()
            {
                log.remove(key);
            }
        }
        for (agent, pos, event) in &expanded {
            let key = (*agent, *pos);
            if old
                .binary_search_by_key(&key, |(key, _)| *key)
                .ok()
                .is_some_and(|index| old[index].1 == *event)
            {
                continue;
            }
            log.insert(&key, SenValue::borrowed(event));
        }
    }
    if !affected.is_empty() {
        let mut rewritten = Vec::with_capacity(journal.len());
        for key in &journal {
            if !affected.contains(&key.0) {
                rewritten.push(*key);
            } else if retained.contains(key) {
                rewritten.extend(added[key].iter().map(|pos| (key.0, *pos)));
            }
        }
        let mut table = write.open_table(JOURNAL);
        for seq in (rewritten.len() + 1) as u64..=journal.len() as u64 {
            table.remove(&seq);
        }
        for (index, key) in rewritten.iter().enumerate() {
            if journal.get(index) != Some(key) {
                table.insert(&((index + 1) as u64), key);
            }
        }
    }
    for agent in next.keys().copied() {
        let head = {
            let log = write.open_table(AGENT_LOG);
            fold_head(super::rows(log.range(super::agent_range(agent))))
                .expect("rewritten log starts with creation")
        };
        write
            .open_table(AGENT_HEADS)
            .insert(&agent, SenValue::borrowed(&head));
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{Seq, UnixMs};
    use rho_db::RhoDb;

    use super::legacy::provider::{Call, CallResult, Carry};
    use super::*;
    use crate::db::{AgentProfileWriteTxnExt, AgentReadTxnExt, AgentWriteTxnExt, FORMAT};
    use crate::entry::{MessageId, Party, Wake};
    use crate::inference::Image;
    use crate::log::{AgentOrigin, SessionBinding};

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
        write.open_table(FORMAT).insert(&(), &"dc371fa2".to_owned());
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
        write.open_table(FORMAT).insert(&(), &"dc371fa2".to_owned());
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
            matches!(&auto[2], Entry::RequestSent { compact: true, imported: None, report, .. } if report.is_empty())
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
            matches!(&manual[1], Entry::RequestSent { compact: true, imported: None, report, .. } if report.is_empty())
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
        let visible_request = crate::worker::native::context::request(
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
            |item| matches!(item, crate::inference::Item::Report { text, .. }
                    if text.contains("survives rewind"))
        ));
        assert!(
            matches!(read.agent_event(ids[3], AgentEventPos::new(5)), Some(AgentEvent::Rewound { to, .. }) if to.pos == 2)
        );
        let request = crate::worker::native::context::request(
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
                        imported: None,
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
                bucket_start_ms: crate::log::AGENT_USAGE_BUCKET_MS,
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
        write.open_table(FORMAT).insert(&(), &"dc371fa2".to_owned());
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
        expected_head.user_interacted = true; // Newly typed Received now folds as user contact.
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
            imported: None,
            compact,
            ..
        }) = &rows[4]
        else {
            panic!("typed report")
        };
        assert!(!compact);
        assert_eq!(report.messages, vec![MessageId(11)]);
        assert_eq!(report.acknowledged, vec![MessageId(12)]);
        assert_eq!(
            report.notebook.render().text,
            "literal text\n\nOutput of earlier exec first:\nalpha\n\nOutput of earlier exec evicted:\nbeta"
        );
        assert_eq!(report.notebook.render().images[0].data, images[0].data);
        assert_eq!(report.notebook.render().images.len(), 1);

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
