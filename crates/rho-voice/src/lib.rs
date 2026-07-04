//! Client for the xAI Grok realtime voice WebSocket protocol.
//!
//! Rho uses Grok voice as a control surface over running agents, not as an
//! inference provider: this crate deliberately speaks its own audio/tool
//! vocabulary and never touches `rho-core` transcript types. The endpoint is
//! OpenAI Realtime API compatible; only the events rho consumes get typed
//! variants, everything else is preserved raw so protocol drift degrades to
//! unknown events instead of failures.

pub mod auth;
pub mod cli;
pub mod session;
pub mod tools;
pub mod wire;

pub use cli::{VoiceArgs, run_voice_cli};
