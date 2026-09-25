//! Sealed append-only byte logs synced through hosts that store only opaque
//! bytes. Devices retain and authenticate each log in byte order; hosts only
//! conditionally append.

#[cfg(feature = "client")]
mod ledger;
pub mod protocol;
#[cfg(feature = "client")]
mod seal;
#[cfg(feature = "client")]
mod secret;
#[cfg(feature = "client")]
pub mod stream;
#[cfg(feature = "client")]
pub use ledger::{Channel, Item, Ledger};
#[cfg(feature = "client")]
pub use secret::Secret;
