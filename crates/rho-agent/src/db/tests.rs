use rho_core::ContentPart;
use rho_db::RhoDb;
use rho_inference::PromptCacheKey;
use rho_inference::config::InferenceConfig;

use super::*;

#[tokio::test]
async fn agent_timeline_refs_sort_by_lineage_then_seq() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    {
        let mut timeline = write.open_table(AGENT_TIMELINE);
        for seq in [2, 0, 1] {
            timeline.insert(
                &AgentTimelineRef {
                    lineage_id: AgentLineageId(7),
                    seq,
                },
                Sen(AgentTimelineEntry {
                    created_at: UnixMillis(u64::from(seq)),
                    context_block: ContextBlock::UserMessage {
                        content: Vec::new(),
                    },
                }),
            );
        }
    }
    write.commit();

    let read = db.read();
    let timeline = read.open_table(AGENT_TIMELINE);
    let seqs = timeline
        .range(
            AgentTimelineRef {
                lineage_id: AgentLineageId(7),
                seq: 0,
            }..=AgentTimelineRef {
                lineage_id: AgentLineageId(7),
                seq: u32::MAX,
            },
        )
        .map(|(key, _)| key.value().seq)
        .collect::<Vec<_>>();

    assert_eq!(seqs, [0, 1, 2]);
}

#[tokio::test]
async fn create_agent_and_append_blocks_with_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let db = RhoDb::open(temp.path().join("rho.redb"));

    let mut write = db.write().await;
    let (agent_id, next) = write.create_agent(
        UnixMillis(1),
        Some("main".to_owned()),
        PromptCacheKey::generate(),
        InferenceConfig::deep(),
    );
    let next = write.append_agent_block(
        next,
        UnixMillis(2),
        Arc::new(ContextBlock::UserMessage {
            content: vec![ContentPart::Text {
                text: "hello".to_owned(),
            }],
        }),
    );
    write.append_agent_block(
        next,
        UnixMillis(3),
        Arc::new(ContextBlock::UserMessage {
            content: vec![ContentPart::Text {
                text: "again".to_owned(),
            }],
        }),
    );
    write.commit();

    let read = db.read();
    let agent = read.get_agent(agent_id);
    assert_eq!(agent.display_name.as_deref(), Some("main"));

    let (next, blocks) = read.agent_blocks(agent_id);
    assert_eq!(next.seq, 2);
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        blocks[0].as_ref(),
        &ContextBlock::UserMessage {
            content: vec![ContentPart::Text {
                text: "hello".to_owned(),
            }],
        }
    );
}
