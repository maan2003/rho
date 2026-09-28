//! Provider-free fixtures for agent and workset tests.
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::mpsc;

use super::{
    Backend, Inference, InferenceHost, InferenceModel, InferenceProfile, InferenceSession,
    PolicySender, Request, Response, Session,
};

struct Fake;

impl Backend for Fake {
    fn session(&self, _: InferenceProfile, _: InferenceModel) -> InferenceSession {
        Arc::new(Fake)
    }
    fn text(&self, _: Arc<str>, _: String) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async { anyhow::bail!("fake inference unavailable") })
    }
    fn web_credentials(&self) -> BoxFuture<'_, anyhow::Result<rho_web_search::Credentials>> {
        Box::pin(async { anyhow::bail!("fake credentials unavailable") })
    }
}

impl Session for Fake {
    fn start(&self, _: Request) -> Response {
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = sender.send(super::Event::Failed(anyhow::anyhow!(
            "fake inference unavailable"
        )));
        receiver
    }
}

pub fn backend() -> Inference {
    Arc::new(Fake)
}

struct FakeHost(Inference);
impl InferenceHost for FakeHost {
    fn client(&self) -> Inference {
        self.0.clone()
    }
    fn responses_base_url(&self) -> &str {
        "http://127.0.0.1:1"
    }
    fn serve_policy(
        &self,
        _: PolicySender,
        mut incoming: mpsc::Receiver<Vec<u8>>,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            while incoming.recv().await.is_some() {}
            Ok(())
        })
    }
}

pub fn accounts() -> super::Accounts {
    Arc::new(FakeHost(backend()))
}
