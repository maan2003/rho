//! Clones born from a shared mirror store (`CLONES.md`): the keeper that
//! holds one bare mirror per remote URL ([`server`]), how the agent host
//! births its own clones from a mirror ([`client`]), and the one-line
//! socket protocol between them ([`protocol`]). The patched `git` agents
//! run speaks that protocol itself.

pub mod client;
pub mod protocol;
pub mod server;
