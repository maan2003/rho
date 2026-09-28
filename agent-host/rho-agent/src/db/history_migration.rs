//! One-time rewrite of all physical branches to a single report-based history.
use std::collections::{BTreeMap, HashMap, HashSet};

use rho_agent_types::AgentId;
use rho_db::{SenValue, WriteTxn};

use super::{AGENT_LOG, AgentRuntime, AgentUsageBucket, JOURNAL, agent_range, rows};
use crate::AgentEvent;
use crate::entry::{Entry, MessageId};

pub(super) fn migrate(write: &mut WriteTxn) {
    eprintln!("rho-agent history migration: scanning old native rows");
    let mut native_failed = BTreeMap::new();
    let mut partial_count = 0usize;
    let mut raw_reasoning = 0usize;
    let mut encrypted_missing = 0usize;
    let mut unknown = 0usize;
    let native_spoken: BTreeMap<(AgentId, u64), Option<AgentUsageBucket>> = write
        .open_table(AGENT_LOG)
        .iter()
        .filter_map(|(key, row)| match row.value().into_owned() {
            AgentEvent::Native(super::legacy::NativeEvent::ResponseFinished { usage, output, .. }) => {
                use rho_agent_types::transcript::{ContextBlock, InferenceResponseItem,
                    OpenAiResponsesProviderData as Provider};
                for block in &output {
                    if let ContextBlock::InferenceResponse { items, .. } = block {
                        for item in items {
                            match item {
                                InferenceResponseItem::RawReasoning { .. } => raw_reasoning += 1,
                                InferenceResponseItem::EncryptedReasoning { provider_specific, .. }
                                    if !matches!(provider_specific.as_any().downcast_ref::<Provider>(),
                                        Some(Provider::EncryptedReasoning { encrypted_content, .. })
                                        if !encrypted_content.is_empty()) => encrypted_missing += 1,
                                InferenceResponseItem::Unknown { .. } | InferenceResponseItem::Compaction { .. }
                                    if super::entries_migration::compaction(item).is_none() => unknown += 1,
                                _ => {}
                            }
                        }
                    }
                }
                Some((key.value(), usage))
            }
            AgentEvent::Native(super::legacy::NativeEvent::RequestFailed {
                at,
                error,
                partial,
                ..
            }) => {
                partial_count += usize::from(!partial.items.is_empty());
                native_failed.insert(key.value(), (at, error));
                None
            }
            _ => None,
        })
        .collect();
    eprintln!(
        "rho-agent history migration: {} native responses, {} failed requests ({} with partial items), {raw_reasoning} raw reasoning, {encrypted_missing} missing encrypted reasoning, {unknown} unknown provider items; converting old native rows",
        native_spoken.len(),
        native_failed.len(),
        partial_count
    );
    let synthetic = super::entries_migration::migrate(write);
    eprintln!("rho-agent history migration: rewriting old row positions");
    super::reports_migration::migrate(write, &native_spoken, &native_failed, &synthetic);
    eprintln!("rho-agent history migration: normalizing reports and cursors");
    // The typed report migration remaps positions and rebuilds the cursor.
    // Normalize every physical branch and rebuild it again after the final rows.
    let agents = write
        .open_table(super::AGENT_HEADS)
        .iter()
        .map(|(key, _)| key.value())
        .collect::<Vec<_>>();
    let mut imported = 0usize;
    let native = native_spoken.len();
    for agent in agents {
        let physical = {
            let log = write.open_table(AGENT_LOG);
            rows(log.range(agent_range(agent))).collect::<Vec<_>>()
        };
        let mut pending = Vec::<MessageId>::new();
        let mut runtime = None::<AgentRuntime>;
        let targets = physical
            .iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Rewound { to, .. } => Some(to.pos),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let mut before = HashMap::<u64, (Vec<MessageId>, Option<AgentRuntime>)>::new();
        for (pos, mut event) in physical {
            if targets.contains(&pos.pos) {
                before.insert(pos.pos, (pending.clone(), runtime.clone()));
            }
            match &mut event {
                AgentEvent::Created {
                    runtime: created, ..
                } => runtime = Some(created.clone()),
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
                    let restored = before
                        .get(&to.pos)
                        .unwrap_or_else(|| {
                            panic!(
                                "agent {} rewinds to absent position {}",
                                agent.encoded(),
                                to.pos
                            )
                        })
                        .clone();
                    (pending, runtime) = restored;
                }
                AgentEvent::Entry(Entry::Received { id, .. }) => {
                    assert!(
                        !pending.contains(id),
                        "agent {} duplicates pending message {:?} at {}",
                        agent.encoded(),
                        id,
                        pos.pos
                    );
                    pending.push(*id);
                }
                AgentEvent::Entry(Entry::RequestSent {
                    report,
                    imported: old,
                    ..
                }) => {
                    report.notebook.migrate_text();
                    if let Some(carry) = old.take() {
                        imported += 1;
                        let value: serde_json::Value = serde_json::from_str(carry.data().get())
                            .expect("historical imported provider JSON");
                        let legacy = &value["imported"];
                        let text = legacy["text"].as_str().expect("historical imported text");
                        let images: Vec<crate::inference::Image> =
                            serde_json::from_value(legacy["images"].clone())
                                .expect("historical imported images");
                        let rendered = report.notebook.render();
                        if rendered.text != text
                            || rendered
                                .images
                                .iter()
                                .zip(&images)
                                .any(|(a, b)| a.media_type != b.media_type || a.data != b.data)
                            || rendered.images.len() != images.len()
                        {
                            report.notebook.merge(rho_notebook::Report::from_text(
                                text.to_owned(),
                                images
                                    .iter()
                                    .map(|i| rho_notebook::Image {
                                        media_type: i.media_type.clone(),
                                        data: i.data.clone(),
                                    })
                                    .collect(),
                            ));
                        }
                        let results = legacy["results"]
                            .as_array()
                            .expect("historical imported results");
                        let one_exact_result = results.len() == 1
                            && rendered.text == text
                            && rendered.images.len() == images.len()
                            && rendered
                                .images
                                .iter()
                                .zip(&images)
                                .all(|(a, b)| a.media_type == b.media_type && a.data == b.data)
                            && results[0]["text"].as_str() == Some(text)
                            && serde_json::from_value::<Vec<crate::inference::Image>>(
                                results[0]["images"].clone(),
                            )
                            .expect("historical result images")
                                == images;
                        if one_exact_result {
                            let id = results[0]["id"].as_str().expect("historical result id");
                            report.notebook = rho_notebook::Report::from_text(
                                format!("Output of earlier exec {id}:\n{text}"),
                                images
                                    .iter()
                                    .map(|i| rho_notebook::Image {
                                        media_type: i.media_type.clone(),
                                        data: i.data.clone(),
                                    })
                                    .collect(),
                            );
                        }
                        for result in results.iter().filter(|_| !one_exact_result) {
                            let id = result["id"].as_str().expect("historical result id");
                            let text = result["text"].as_str().expect("historical result text");
                            let images: Vec<crate::inference::Image> =
                                serde_json::from_value(result["images"].clone())
                                    .expect("historical result images");
                            let existing = report.notebook.render().images;
                            let mut distinct = Vec::new();
                            for image in images {
                                if !existing.iter().any(|prior| {
                                    prior.media_type == image.media_type && prior.data == image.data
                                }) && !distinct.iter().any(|prior: &rho_notebook::Image| {
                                    prior.media_type == image.media_type && prior.data == image.data
                                }) {
                                    distinct.push(rho_notebook::Image {
                                        media_type: image.media_type,
                                        data: image.data,
                                    });
                                }
                            }
                            report.notebook.merge(rho_notebook::Report::from_text(
                                format!("Output of earlier exec {id}:\n{text}"),
                                distinct,
                            ));
                        }
                    } else if matches!(runtime, Some(AgentRuntime::Rho { .. })) {
                        // Native Rho sends consumed the whole pending interval.
                        // Claude's explicit acknowledgment must stay unchanged.
                        report.messages = pending.clone();
                        report.acknowledged.clear();
                    }
                    let mut seen = HashSet::new();
                    for id in report.messages.iter().chain(&report.acknowledged) {
                        assert!(
                            seen.insert(*id) && pending.contains(id),
                            "agent {} send {} references duplicate or absent pending message {:?}",
                            agent.encoded(),
                            pos.pos,
                            id
                        );
                    }
                    pending.retain(|id| !seen.contains(id));
                    write
                        .open_table(AGENT_LOG)
                        .insert(&(agent, pos.pos), SenValue::borrowed(&event));
                }
                AgentEvent::Native(_) | AgentEvent::LegacyEntry(_) => {
                    panic!("agent {} has unconverted row {}", agent.encoded(), pos.pos)
                }
                _ => {}
            }
        }
        let cursor = super::native::rebuild(write, agent);
        write
            .open_table(super::native::NATIVE_CURSORS)
            .insert(&agent, SenValue::borrowed(&cursor));
    }
    // Every journal entry must name a physical row after any position remap.
    let journal = write
        .open_table(JOURNAL)
        .iter()
        .map(|(_, target)| target.value())
        .collect::<Vec<_>>();
    for key in journal {
        assert!(
            write.open_table(AGENT_LOG).get(&key).is_some(),
            "journal references missing row {key:?}"
        );
    }
    eprintln!(
        "rho-agent history migration: {native} native responses, {imported} imported reports converted across all physical branches"
    );
}
