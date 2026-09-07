//! A Slack that is real enough to live in.
//!
//! Not a test double bolted to one client: a server with its own typed store,
//! its own seeded workspace, and its own opinions about what Slack answers
//! when a call is wrong. Clients connect to it — one `rho-slack` session, or
//! several, or a whole GUI — and it is the same server in a test as it is
//! under the rig.
//!
//! This is the first landing: the store, the seeded world, the read side of
//! the web API, and one client's full sync measured against it. The socket
//! and the living schedule come next, then several clients at once with the
//! server checking that what they see agrees.
//!
//! ```no_run
//! # async fn show() -> anyhow::Result<()> {
//! use rho_fake_slack::{FakeSlack, Seed};
//! let slack = FakeSlack::start(Seed::default()).await?;
//! let api = slack.api_base();
//! # let _ = api;
//! # Ok(())
//! # }
//! ```

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::post;

pub mod api;
pub mod store;
pub mod types;
pub mod wire;
pub mod world;

pub use api::{Action, Method, Refusal};
pub use store::Store;
pub use world::{SELF_ID, Seed};

/// A running server, and the handle that stops it. Dropping it takes the
/// workspace down with it: the listener closes and the tasks end, so a test
/// that forgets to stop it does not leave one behind.
pub struct FakeSlack {
    store: Arc<Store>,
    control: Arc<Mutex<api::Control>>,
    api_base: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeSlack {
    /// Builds the world from the seed and starts serving it on a loopback
    /// port, returning once the port is accepting so a client can connect
    /// immediately.
    pub async fn start(seed: Seed) -> anyhow::Result<Self> {
        Self::serve(world::build(seed)).await
    }

    /// Starts on a workspace built elsewhere — the seam eng-8gpr's generator
    /// arrives through, so this crate carries no fixture of its own.
    pub async fn serve(store: Store) -> anyhow::Result<Self> {
        let store = Arc::new(store);
        let control = Arc::new(Mutex::new(api::Control::default()));
        let server = api::Server {
            store: store.clone(),
            control: control.clone(),
        };
        let router = Router::new()
            .route("/api/{method}", post(api::call))
            .with_state(server);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let api_base = format!("http://{}/api", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            store,
            control,
            api_base,
            task,
        })
    }

    /// Where the web API is, in the form a client is configured with.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// The workspace, for a test that wants to assert against the truth
    /// rather than against what a client made of it.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Does something to the server: the same typed action the binary's
    /// control endpoint takes.
    pub fn take(&self, action: Action) {
        self.control.lock().expect("control").take(action);
    }

    /// How many requests have been answered, in total and per method — the
    /// numbers a landing note is made of.
    pub fn served(&self) -> u64 {
        self.control.lock().expect("control").served_total()
    }

    pub fn served_method(&self, method: Method) -> u64 {
        self.control.lock().expect("control").served(method)
    }
}

impl Drop for FakeSlack {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests;
