use rho::{Item, Role};

use super::*;

#[tokio::test]
async fn appends_and_reads_blocks() {
    let path = std::env::temp_dir().join(format!(
        "rho-cbor-log-{}-{}.cbor",
        std::process::id(),
        "appends_and_reads_blocks"
    ));
    let _ = fs::remove_file(&path).await;

    let log = CborLog::new(&path);
    let item = Item::message("item-1", Role::User, "hello");
    let block = ItemBlock::Local { items: vec![item] };

    log.append_block(&block).await.unwrap();
    let blocks = log.read_blocks().await.unwrap();

    assert_eq!(blocks, vec![block]);
    let _ = fs::remove_file(&path).await;
}
