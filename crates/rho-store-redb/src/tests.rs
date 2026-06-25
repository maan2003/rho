use rho_core::{ContentPart, ContextBlock, InferenceResponseItem};

use super::*;
use crate::node_id::decode_ordvarint;

fn user_block(text: &str) -> ContextBlock {
    ContextBlock::UserMessage {
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
    }
}

fn assistant_response(text: &str) -> ContextBlock {
    ContextBlock::InferenceResponse {
        provider_response_id: None,
        items: vec![InferenceResponseItem::AssistantMessage {
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
        }],
    }
}

#[test]
fn ordered_varints_roundtrip_and_sort_by_numeric_value() {
    let values = [
        0,
        1,
        240,
        241,
        2_287,
        2_288,
        67_823,
        67_824,
        0xFF_FFFF,
        0x1_000000,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        u64::MAX,
    ];

    let mut encoded = Vec::new();
    for value in values {
        let bytes = encode_ordvarint(value);
        assert_eq!(decode_ordvarint(&bytes).unwrap(), (value, bytes.len()));
        encoded.push((bytes, value));
    }

    encoded.sort_by(|left, right| left.0.cmp(&right.0));
    let sorted_values: Vec<u64> = encoded.into_iter().map(|(_bytes, value)| value).collect();
    assert_eq!(sorted_values, values);
}

#[test]
fn ordered_varints_reject_non_canonical_aliases() {
    assert!(decode_ordvarint(&[241, 0]).is_err());
    assert!(decode_ordvarint(&[250, 0, 0, 1]).is_err());
}

#[test]
fn node_ref_encoding_has_no_length_prefix() {
    assert_eq!(encode_node_key(NodeRef::new(0, 0)), vec![0, 0]);
    assert_eq!(encode_node_key(NodeRef::new(241, 2)), vec![241, 1, 2]);
}

#[test]
fn postcard_node_ref_encoding_has_no_length_prefix() {
    assert_eq!(encode_value(&NodeRef::new(0, 0)).unwrap(), vec![0, 0]);
    assert_eq!(
        encode_value(&NodeRef::new(241, 2)).unwrap(),
        encode_node_key(NodeRef::new(241, 2))
    );
    assert_eq!(
        decode_value::<NodeRef>(&[0, 0]).unwrap(),
        NodeRef::new(0, 0)
    );
    assert_eq!(
        decode_value::<NodeRef>(&[241, 1, 2]).unwrap(),
        NodeRef::new(241, 2)
    );
}

#[tokio::test]
async fn appends_linear_branch_in_one_lineage() {
    let temp = tempfile::tempdir().unwrap();
    let store = RedbStore::open(temp.path().join("items.redb"))
        .await
        .unwrap();
    let first = user_block("hello");
    let second = assistant_response("hi");

    let first_ref = store.append_child(None, &first).await.unwrap();
    let second_ref = store.append_child(Some(first_ref), &second).await.unwrap();

    assert_eq!(first_ref, NodeRef::new(0, 0));
    assert_eq!(second_ref, NodeRef::new(0, 1));
    assert_eq!(
        store.read_branch(second_ref).await.unwrap(),
        vec![(first_ref, first), (second_ref, second)]
    );
}

#[tokio::test]
async fn fork_from_interior_node_uses_new_lineage_and_reads_shared_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let store = RedbStore::open(temp.path().join("items.redb"))
        .await
        .unwrap();
    let root = user_block("root");
    let main = assistant_response("main");
    let fork = assistant_response("fork");

    let root_ref = store.append_child(None, &root).await.unwrap();
    let main_ref = store.append_child(Some(root_ref), &main).await.unwrap();
    let fork_ref = store.append_child(Some(root_ref), &fork).await.unwrap();

    assert_eq!(main_ref, NodeRef::new(0, 1));
    assert_eq!(fork_ref, NodeRef::new(1, 0));
    assert_eq!(
        store.read_branch(fork_ref).await.unwrap(),
        vec![(root_ref, root), (fork_ref, fork)]
    );
}

#[tokio::test]
async fn agent_refs_list_without_touching_payload_shape() {
    let temp = tempfile::tempdir().unwrap();
    let store = RedbStore::open(temp.path().join("items.redb"))
        .await
        .unwrap();
    let first_ref = store
        .append_child(None, &user_block("hello"))
        .await
        .unwrap();

    let agent = store
        .create_agent(Some(first_ref), Some("conversation".to_owned()))
        .await
        .unwrap();
    let second_ref = store
        .append_agent_block(agent.id, &assistant_response("hi"))
        .await
        .unwrap();

    let listed = store.list_agents().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, agent.id);
    assert_eq!(listed[0].head, Some(second_ref));
    assert_eq!(
        store
            .read_agent_branch(agent.id)
            .await
            .unwrap()
            .into_iter()
            .map(|(node_ref, _block)| node_ref)
            .collect::<Vec<_>>(),
        vec![first_ref, second_ref]
    );
}

#[tokio::test]
async fn reopens_existing_database() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("items.redb");
    let first = user_block("hello");
    let second = assistant_response("hi");

    let store = RedbStore::open(&path).await.unwrap();
    let first_ref = store.append_child(None, &first).await.unwrap();
    drop(store);

    let reopened = RedbStore::open(&path).await.unwrap();
    let second_ref = reopened
        .append_child(Some(first_ref), &second)
        .await
        .unwrap();

    assert_eq!(
        reopened.read_branch(second_ref).await.unwrap(),
        vec![(first_ref, first), (second_ref, second)]
    );
}

#[tokio::test]
async fn redb_log_compatibility_uses_default_agent() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("items.redb");
    let log = RedbLog::open(&path).await.unwrap();
    let first = user_block("hello");
    let second = assistant_response("hi");

    log.append_block(&first).await.unwrap();
    log.append_block(&second).await.unwrap();

    assert_eq!(log.read_blocks().await.unwrap(), vec![first, second]);
}
