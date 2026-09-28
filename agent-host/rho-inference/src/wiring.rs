//! The concrete provider adapter for agent-owned inference and workset policy.
use std::sync::Arc;

use rho_agent::inference::{InferenceHost, PolicySender, WorkerFactory, WorkerInference};
use tokio::sync::mpsc;

use crate::{Accounts, InferenceConfig};

impl InferenceHost for Accounts {
    fn client(&self) -> rho_agent::inference::Inference {
        Arc::new(self.client())
    }

    fn responses_base_url(&self) -> &str {
        self.responses_base_url()
    }

    fn serve_policy(
        &self,
        sender: PolicySender,
        incoming: mpsc::Receiver<Vec<u8>>,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(crate::policy::serve(self.clone(), sender, incoming))
    }
}

fn worker_factory(base_url: &str, sender: PolicySender) -> anyhow::Result<WorkerInference> {
    let policy = crate::policy::Host::new(sender);
    let (inference, mut calls) =
        policy.inference(InferenceConfig::with_responses_base_url(base_url)?);
    let worker_policy = policy.clone();
    tokio::spawn(async move {
        let mut pending = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = worker_policy.closed() => break,
                Some(call) = calls.recv(), if pending.len() < crate::policy::MAX_REQUESTS => {
                    let policy = worker_policy.clone();
                    pending.spawn(async move {
                        let result = policy.request(call.body).await;
                        let _ = call.reply.send(result);
                    });
                }
                Some(_) = pending.join_next(), if !pending.is_empty() => {}
                else => break,
            }
        }
        pending.abort_all();
    });
    Ok(WorkerInference {
        inference: Arc::new(inference),
        policy,
    })
}

/// Start the companion worker with the provider's policy and model transport.
pub fn worker_main() -> anyhow::Result<()> {
    rho_agent::worker_main(worker_factory as WorkerFactory)
}
