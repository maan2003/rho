//! rho-gui: the one window over the frame and the systems.
//!
//! The frame (`rho-frame`) owns everything every screen shares; each
//! system (`rho-chat`, …) owns its own surfaces, commands and cards. This crate
//! lists the systems, opens the window and holds what belongs to the
//! desktop and nothing else: fonts and themes ([`rho_assets`]), the chime
//! ([`chime`]) and the D-Bus name ([`application`]).

pub mod application;
pub mod chime;
pub mod rho_assets;
pub mod telemetry;

#[cfg(test)]
mod command_ranking;
