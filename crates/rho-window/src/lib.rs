//! The window's own vocabulary: what every screen is drawn with.
//!
//! A screen in Rho is a buffer. What makes one readable — the classes a
//! span can carry, the colours and blocks they turn into, the highlights
//! laid over a multibuffer, the configuration an editor is opened with —
//! is the same in every screen, so it lives here rather than in any one
//! of them. Source crates use it and add no primitives of their own
//! (`GUI-CRATES-DESIGN.md`).
//!
//! This crate names nothing above it: it knows buffers and editors, not
//! agents, Slack or the desk.

pub mod editor_config;
pub mod highlights;
pub mod languages;
pub mod markdown;
pub mod style;
pub mod transient;
pub mod visualization;

pub use style::{Region, StyleClass};
