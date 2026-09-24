//! The desk the client held before the ledger, read once by the migration
//! that moves it into the ledger. [`protocol`] is the desk's own types,
//! [`cache`] the tables the client kept them in, and [`export`] reads them
//! out whole.

pub(crate) mod cache;
pub mod export;
pub mod protocol;
