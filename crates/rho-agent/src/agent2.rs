use rho_core::IInferenceSession;

use crate::invariants::AgentInvariantsEnforcer;

struct Agent {
    provider_session: Box<dyn IInferenceSession>,
    history: AgentInvariantsEnforcer,
    agent_status: AgentStateKind,
}

enum AgentStateKind {
    // This is a temporary error, and can be retried if needed, doesn't affect the persisted
    // thread state! TODO: better error clasification
    Error(anyhow::Error),
}

impl Agent {
    async fn run(&mut self) {
        loop {
            tokio::select! {
                biased;
                response = self.provider_session.run() => {
                    match response {
                        Ok(rho_core::InferenceUpdate::Finished(finished)) => todo!(),
                        Ok(_) => {
                            // TODO: handle intermediate events
                        }
                        Err(e) => self.agent_status = AgentStateKind::Error(e),
                    }

                }
            }
        }
    }
}
