//! Ordered native event replication. The live loop owns its unflushed tail;
//! only complete transactions become recovery authority.
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::AgentEvent;
use crate::worker::{Host, StoreError};

enum Write {
    Events(Vec<AgentEvent<'static>>),
    Flush(oneshot::Sender<()>),
}

pub(super) struct Writer {
    queue: mpsc::Sender<Write>,
}

impl Writer {
    pub fn new(host: Arc<Host>) -> Self {
        let (queue, mut incoming) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(write) = incoming.recv().await {
                match write {
                    Write::Events(events) => {
                        if let Err(error) = host.append_batch(events).await {
                            eprintln!("native event replication failed: {error}");
                            // Dropping the receiver wakes the loop and all barriers.
                            return;
                        }
                    }
                    Write::Flush(done) => {
                        let _ = done.send(());
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

    pub async fn flush(&self) -> Result<(), StoreError> {
        let (done, finished) = oneshot::channel();
        self.queue
            .send(Write::Flush(done))
            .await
            .map_err(|_| StoreError(anyhow::anyhow!("native event replication stopped")))?;
        finished
            .await
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
