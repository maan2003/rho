//! The GCCP hosts the user approved: other harnesses whose clankers may
//! message this host's agents ([docs/gccp.md](../../../docs/gccp.md)).
//!
//! A GCCP host is not a client. It has a table of its own, and nothing that
//! decides whether an endpoint is a client reads it: no approval here can
//! become access to the host.
//!
//! The functions take a transaction, so a caller can change what depends on
//! an approval together with it.

use redb::TableDefinition;
use rho_db::{ReadTxn, WriteTxn};

/// An endpoint id: the name the user gave the host there.
const GCCP_HOSTS: TableDefinition<&[u8; 32], String> =
    TableDefinition::new("rho_iroh_auth_gccp_hosts_v1");

/// Makes the table, so that a read finds it.
pub fn init(write: &mut WriteTxn) {
    write.open_table(GCCP_HOSTS);
}

/// The name of the approved host at `endpoint`.
pub fn name(read: &ReadTxn, endpoint: &[u8; 32]) -> Option<String> {
    read.open_table(GCCP_HOSTS)
        .get(endpoint)
        .map(|name| name.value())
}

/// [`name`], inside a write.
pub fn name_in(write: &mut WriteTxn, endpoint: &[u8; 32]) -> Option<String> {
    write
        .open_table(GCCP_HOSTS)
        .get(endpoint)
        .map(|name| name.value())
}

/// Every approved host, with its name.
pub fn all(read: &ReadTxn) -> Vec<([u8; 32], String)> {
    read.open_table(GCCP_HOSTS)
        .iter()
        .map(|(endpoint, name)| (*endpoint.value(), name.value()))
        .collect()
}

/// Approves the host at `endpoint` under `name`, or renames it.
pub fn approve(write: &mut WriteTxn, endpoint: &[u8; 32], name: String) {
    write.open_table(GCCP_HOSTS).insert(endpoint, name);
}

/// Takes an approval back. Says whether there was one.
pub fn revoke(write: &mut WriteTxn, endpoint: &[u8; 32]) -> bool {
    write.open_table(GCCP_HOSTS).remove(endpoint).is_some()
}
