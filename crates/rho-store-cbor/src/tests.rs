use rho_core::{ContentPart, ContextBlock};

use super::*;

fn user_block(text: &str) -> ContextBlock {
    ContextBlock::UserMessage {
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
    }
}

#[tokio::test]
async fn appends_and_reads_blocks() {
    let path = std::env::temp_dir().join(format!(
        "rho-cbor-log-{}-{}.cbor",
        std::process::id(),
        "appends_and_reads_blocks"
    ));
    let _ = fs::remove_file(&path).await;

    let log = CborLog::new(&path);
    let block = user_block("hello");

    log.append_block(&block).await.unwrap();
    let blocks = log.read_blocks().await.unwrap();

    assert_eq!(blocks, vec![block]);
    let _ = fs::remove_file(&path).await;
}
