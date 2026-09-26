//! Agent2's host wire: chat messages and status only; no notebook internals.
pub mod protocol;
#[cfg(feature = "client")]
pub mod remote;
#[cfg(feature = "client")]
pub mod stream;
