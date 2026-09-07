//! A Slack that is real enough to live in.
//!
//! Not a test double bolted to one client: a server with its own typed store,
//! its own seeded workspace, and its own opinions about what Slack answers
//! when a call is wrong. Clients connect to it — one `rho-slack` session, or
//! several, or a whole GUI — and it is the same server in a test as it is
//! under the rig.
//!
//! Two landings so far: the store with the seeded world and the read side of
//! the web API, and now the socket with time running in it — other people
//! post, reply, react, edit and read on a schedule that comes out of the
//! same seed, and every write becomes a frame every connected client sees.
//! Several clients at once, with the server checking that what they see
//! agrees, comes next.
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

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::routing::{any, post};

pub mod api;
pub mod live;
pub mod observe;
pub mod socket;
pub mod store;
pub mod types;
pub mod wire;
pub mod world;

pub use api::{Action, Method, Refusal};
pub use live::{Happening, Living, Schedule};
pub use observe::{ClientView, Disagreement, Observations};
pub use socket::Wire;
pub use store::Store;
pub use world::{SELF_ID, Seed};

/// A running server, and the handle that stops it. Dropping it takes the
/// workspace down with it: the listener closes and the tasks end, so a test
/// that forgets to stop it does not leave one behind.
pub struct FakeSlack {
    store: Arc<RwLock<Store>>,
    control: Arc<Mutex<api::Control>>,
    live: Wire,
    /// Time, shared with the control endpoint so that a rate started over
    /// HTTP and an `advance` from a test draw from one sequence.
    living: Living,
    api_base: String,
    socket_base: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeSlack {
    /// Builds the world from the seed and starts serving it on a loopback
    /// port, returning once the port is accepting so a client can connect
    /// immediately.
    pub async fn start(seed: Seed) -> anyhow::Result<Self> {
        Self::serve(world::build(seed)).await
    }

    /// The same, with time already running at `per_second` happenings.
    pub async fn start_living(seed: Seed, per_second: f64) -> anyhow::Result<Self> {
        let slack = Self::start(seed).await?;
        slack.live(per_second);
        Ok(slack)
    }

    /// Starts on a workspace built elsewhere — the seam eng-8gpr's generator
    /// arrives through, so this crate carries no fixture of its own.
    pub async fn serve(store: Store) -> anyhow::Result<Self> {
        Self::serve_on("127.0.0.1:0", store).await
    }

    /// The same, on an address the caller picks — what the binary form needs
    /// so the rig can point rho at a port it knows.
    pub async fn serve_on(address: &str, store: Store) -> anyhow::Result<Self> {
        let schedule = Schedule::new(&store, store_seed(&store));
        let store = Arc::new(RwLock::new(store));
        let control = Arc::new(Mutex::new(api::Control::default()));
        let live = Wire::default();
        let living = Living::new(schedule, store.clone(), live.clone());
        let server = api::Server {
            store: store.clone(),
            control: control.clone(),
            live: live.clone(),
            living: living.clone(),
        };
        let router = Router::new()
            .route("/api/{method}", post(api::call))
            // The upgrade is a GET, so it cannot share the API's route.
            .route("/socket", any(socket::connect))
            // The control surface, for the binary form: the same typed
            // action the in-process handle takes, and the same observations.
            .route("/control", post(api::control).get(api::watched))
            .with_state(server);
        let listener = tokio::net::TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let api_base = format!("http://{address}/api");
        let socket_base = format!("ws://{address}/socket");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            store,
            control,
            live,
            living,
            api_base,
            socket_base,
            task,
        })
    }

    /// Where the web API is, in the form a client is configured with.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Where the socket is. `rtm.connect` says the same thing; this is for
    /// the binary's own banner and for a rig that wants to say it aloud.
    pub fn socket_base(&self) -> &str {
        &self.socket_base
    }

    /// The workspace, for a test that wants to assert against the truth
    /// rather than against what a client made of it. Held shared, so a
    /// caller that keeps the guard holds up the schedule; take what you need
    /// and drop it.
    pub fn store(&self) -> std::sync::RwLockReadGuard<'_, Store> {
        self.store.read().expect("store")
    }

    /// Runs the next `happenings` right now and hands back what they were.
    /// No sleeping and no timer: the same call with the same count does the
    /// same thing every run, which is what makes a test against a living
    /// workspace repeatable.
    pub fn advance(&self, happenings: usize) -> Vec<Happening> {
        self.living.advance(happenings)
    }

    /// How many happenings have been applied since the server started, by
    /// `advance` and by the rate alike.
    pub fn happenings(&self) -> u64 {
        self.living.happenings()
    }

    /// Starts time at a rate, or changes it. The same sequence `advance`
    /// would produce, spread over a clock instead of a loop: a rate is when
    /// they happen, not what happens.
    pub fn live(&self, per_second: f64) {
        self.living.rate(per_second);
    }

    /// Stops time without stopping the server.
    pub fn still(&self) {
        self.living.rate(0.0);
    }

    /// How many clients are on the socket.
    pub fn connected(&self) -> usize {
        self.live.connected()
    }

    /// What the server saw every client be told, and everywhere they do not
    /// agree. A snapshot: a client that simply has not been handed the last
    /// frame yet reads as `Behind`, which is what `settled` waits out.
    pub fn observations(&self) -> Observations {
        self.live.observations(&self.store.read().expect("store"))
    }

    /// Waits until every connected client has been handed everything
    /// published, or the wait runs out. Returns whether they caught up, so a
    /// test can say so rather than assume it.
    pub async fn settled(&self, within: Duration) -> bool {
        let until = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < until {
            if self.live.caught_up() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        self.live.caught_up()
    }

    /// Does something to the server: the same typed action the binary's
    /// control endpoint takes, over the same enum. Hands back what happened
    /// when the action was one that makes things happen.
    pub fn take(&self, action: Action) -> Vec<Happening> {
        self.control.lock().expect("control").take(action);
        self.living.take(action)
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
        // Everything this started stops here: the listener, the sockets it
        // is holding open, and time itself. A test that forgets is not
        // leaving a workspace running behind it.
        self.task.abort();
        self.living.stop();
    }
}

/// The number the schedule is seeded with, taken from the world rather than
/// passed alongside it, so that `serve` on someone else's store — the
/// generator's, when it lands — still replays.
fn store_seed(store: &Store) -> u64 {
    let mut seed = store.conversation_count() as u64;
    seed = seed
        .wrapping_mul(1_000_003)
        .wrapping_add(store.message_count() as u64);
    seed.wrapping_mul(1_000_003)
        .wrapping_add(store.user_count() as u64)
}

#[cfg(test)]
mod tests;
