//! Ordered native event replication. The live loop owns its unflushed tail;
//! only complete transactions become recovery authority.
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::AgentEvent;
use crate::db::ContextBoundary;
use crate::worker::{Host, StoreError};

enum Write {
    Events(Vec<AgentEvent<'static>>),
    Flush(oneshot::Sender<ContextBoundary>),
}

#[derive(Clone)]
pub(super) struct Writer {
    queue: mpsc::Sender<Write>,
}

impl Writer {
    pub fn new(host: Arc<Host>, mut boundary: ContextBoundary) -> Self {
        let (queue, mut incoming) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(write) = incoming.recv().await {
                match write {
                    Write::Events(events) => {
                        match host.append_batch(events).await {
                            Ok(committed) => boundary = committed,
                            Err(error) => {
                                eprintln!("native event replication failed: {error}");
                                // Dropping the receiver wakes the loop and all barriers.
                                return;
                            }
                        }
                    }
                    Write::Flush(done) => {
                        let _ = done.send(boundary);
                    }
                }
            }
        });
        Self { queue }
    }

    pub async fn append(&self, events: Vec<AgentEvent<'static>>) -> Result<(), StoreError> {
        self.queue
            .send(Write::Events(events))
            .await
            .map_err(|_| StoreError(anyhow::anyhow!("native event replication stopped")))
    }

    /// Enqueue a barrier now, without waiting for disk. Its reply pins
    /// precisely this tail even if more events arrive before the caller
    /// needs full replay.
    pub async fn checkpoint(&self) -> Result<oneshot::Receiver<ContextBoundary>, StoreError> {
        let (done, finished) = oneshot::channel();
        self.queue
            .send(Write::Flush(done))
            .await
            .map_err(|_| StoreError(anyhow::anyhow!("native event replication stopped")))?;
        Ok(finished)
    }

    pub async fn flush(&self) -> Result<(), StoreError> {
        self.checkpoint()
            .await?
            .await
            .map(|_| ())
            .map_err(|_| StoreError(anyhow::anyhow!("native event replication stopped")))
    }

    pub fn check(&self) -> Result<(), StoreError> {
        if self.queue.is_closed() {
            return Err(StoreError(anyhow::anyhow!(
                "native event replication stopped"
            )));
        }
        Ok(())
    }

    pub async fn failed(&self) -> StoreError {
        self.queue.closed().await;
        StoreError(anyhow::anyhow!("native event replication stopped"))
    }
}
