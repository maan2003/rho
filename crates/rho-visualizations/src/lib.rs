//! Immutable visualization artifacts stored in RhoDB.

use redb::TableDefinition;
use rho_core::UnixMs;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};
use sha2::{Digest as _, Sha256};

pub const SVG_MIME_TYPE: &str = "image/svg+xml";
pub const MAX_VISUALIZATION_BYTES: usize = 4 * 1024 * 1024;

const VISUALIZATIONS: TableDefinition<String, Sen<Visualization>> =
    TableDefinition::new("visualizations");

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Visualization {
    pub created_at: UnixMs,
    pub mime_type: String,
    pub content: Vec<u8>,
}

/// Opaque immutable visualization storage over the daemon's RhoDB.
#[derive(Clone)]
pub struct VisualizationStore {
    db: RhoDb,
}

impl VisualizationStore {
    pub async fn new(db: RhoDb) -> Self {
        let mut write = db.write().await;
        write.open_table(VISUALIZATIONS);
        write.commit();
        Self { db }
    }

    pub async fn record(&self, mime_type: String, content: Vec<u8>) -> anyhow::Result<String> {
        anyhow::ensure!(
            content.len() <= MAX_VISUALIZATION_BYTES,
            "visualization is too large (maximum {MAX_VISUALIZATION_BYTES} bytes)"
        );
        let id = visualization_id(&mime_type, &content);
        let visualization = Visualization {
            created_at: UnixMs::now(),
            mime_type,
            content,
        };
        let mut write = self.db.write().await;
        {
            let mut table = write.open_table(VISUALIZATIONS);
            if let Some(existing) = table.get(&id) {
                let existing = existing.value().into_owned();
                anyhow::ensure!(
                    existing.mime_type == visualization.mime_type
                        && existing.content == visualization.content,
                    "visualization id collision"
                );
                return Ok(id);
            }
            table.insert(&id, SenValue::borrowed(&visualization));
        }
        write.commit();
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<Visualization> {
        if !is_visualization_id(id) {
            return None;
        }
        self.db
            .read()
            .open_table(VISUALIZATIONS)
            .get(&id.to_owned())
            .map(|value| value.value().into_owned())
    }
}

fn visualization_id(mime_type: &str, content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rho-visualization-v1\0");
    hasher.update(mime_type.as_bytes());
    hasher.update(b"\0");
    hasher.update(content);
    let digest = hasher.finalize();
    let mut id = String::with_capacity(32);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in &digest[..16] {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    id
}

fn is_visualization_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;

    use super::*;

    #[tokio::test]
    async fn records_an_immutable_snapshot_without_inspecting_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = VisualizationStore::new(RhoDb::open(temp.path().join("rho.redb"))).await;
        let source = b"this is not valid SVG".to_vec();
        let id = store
            .record(SVG_MIME_TYPE.to_owned(), source.clone())
            .await
            .unwrap();
        let duplicate = store
            .record(SVG_MIME_TYPE.to_owned(), source.clone())
            .await
            .unwrap();

        assert_eq!(duplicate, id);
        let visualization = store.get(&id).unwrap();
        assert_eq!(visualization.content, source);
        assert_eq!(store.get("missing"), None);
    }

    #[tokio::test]
    async fn refuses_an_oversized_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = VisualizationStore::new(RhoDb::open(temp.path().join("rho.redb"))).await;
        let error = store
            .record(
                SVG_MIME_TYPE.to_owned(),
                vec![0; MAX_VISUALIZATION_BYTES + 1],
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("visualization is too large"));
    }
}
