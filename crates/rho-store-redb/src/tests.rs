use rho_core::{ContextBlock, ContextItem, Role};

use super::*;

#[tokio::test]
async fn appends_and_reads_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("items.redb");
    let log = RedbLog::open(&path).await.unwrap();
    let first = ContextItem::message("item-1", Role::User, "hello");
    let second = ContextItem::message("item-2", Role::Assistant, "hi");
    let first_block = ContextBlock::Local { items: vec![first] };
    let second_block = ContextBlock::Local {
        items: vec![second],
    };

    log.append_block(&first_block).await.unwrap();
    log.append_block(&second_block).await.unwrap();

    assert_eq!(
        log.read_blocks().await.unwrap(),
        vec![first_block, second_block]
    );
}

#[tokio::test]
async fn reopens_existing_database() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("items.redb");
    let first = ContextItem::message("item-1", Role::User, "hello");
    let second = ContextItem::message("item-2", Role::Assistant, "hi");
    let first_block = ContextBlock::Local { items: vec![first] };
    let second_block = ContextBlock::Local {
        items: vec![second],
    };

    RedbLog::open(&path)
        .await
        .unwrap()
        .append_block(&first_block)
        .await
        .unwrap();
    let reopened = RedbLog::open(&path).await.unwrap();
    reopened.append_block(&second_block).await.unwrap();

    assert_eq!(
        reopened.read_blocks().await.unwrap(),
        vec![first_block, second_block]
    );
}
