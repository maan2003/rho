//! Agent-host lifecycle, subprocess supervision, and shared service handlers.

mod agent_client;
pub mod pool;
mod process;
pub(crate) mod services;
mod workset_client;

pub use agent_client::AgentClient;
pub use process::Process;
pub use workset_client::Client as WorksetClient;
