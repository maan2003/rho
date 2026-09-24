//! The ledger: everything the user put into rho that has to outlive a
//! restart and reach their other devices. Most of what rho shows is built
//! at runtime from its sources; the ledger holds only the rest.
//!
//! It is a map from keys to values, merged last-writer-wins. Each device
//! writes its own entries into segments of its own, seals them with a key
//! derived from the [`Secret`] only the user's devices hold, and publishes them
//! to every host it reaches. A host keeps each device's segments and passes
//! them on: it can neither read nor forge them, and it merges nothing. Every
//! device reads every other device's segments and merges them itself.
//!
//! [`protocol`] is what a device and a host say; the host uses it alone,
//! without the `client` feature. [`Ledger`] is a device's side: its own
//! entries, what it merged, and the segments it publishes. [`stream`]
//! carries segments to and from each host.

pub mod protocol;

#[cfg(feature = "client")]
mod entry;
#[cfg(feature = "client")]
mod ledger;
#[cfg(feature = "client")]
mod seal;
#[cfg(feature = "client")]
mod secret;
#[cfg(feature = "client")]
pub mod stream;

#[cfg(feature = "client")]
pub use entry::{Entry, Stamp};
#[cfg(feature = "client")]
pub use ledger::{Change, Ledger, Received};
#[cfg(feature = "client")]
pub use secret::Secret;
