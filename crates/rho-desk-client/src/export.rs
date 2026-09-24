//! The desk this client held, read out whole so it can move into the
//! ledger. Read once, by the migration that replaces the desk.

use std::collections::BTreeMap;

use rho_db::RhoDb;

use crate::protocol::cells::{Id, Property, PropertyKey};

/// One thing on the desk: every claim held about it and, for a note, its
/// text.
#[derive(Debug, Default)]
pub struct HeldNode {
    pub properties: Vec<Property>,
    pub body: Option<String>,
}

/// Everything the replica holds, over every host it held. A thing held by
/// more than one host takes each claim from the host that held the most
/// cells, which is the one the user worked on.
pub fn held(db: &RhoDb) -> BTreeMap<Id, HeldNode> {
    let read = db.read();
    if !read.has_table("gui_desk_cells_v1") {
        return BTreeMap::new();
    }
    let cells = read.open_table(super::cache::DESK_CELLS);
    let mut by_host: BTreeMap<String, Vec<crate::protocol::cells::Cell>> = BTreeMap::new();
    for (key, value) in cells.iter() {
        by_host
            .entry(key.value().into_owned().host)
            .or_default()
            .push(value.value().into_owned());
    }
    let mut hosts: Vec<_> = by_host.into_iter().collect();
    hosts.sort_by_key(|(_, cells)| std::cmp::Reverse(cells.len()));
    let mut claims: BTreeMap<(Id, PropertyKey), Property> = BTreeMap::new();
    for (_, cells) in &hosts {
        for cell in cells {
            claims
                .entry((cell.id.clone(), cell.property.key()))
                .or_insert_with(|| cell.property.clone());
        }
    }
    let mut nodes: BTreeMap<Id, HeldNode> = BTreeMap::new();
    for ((id, _), property) in claims {
        nodes.entry(id).or_default().properties.push(property);
    }
    if read.has_table("gui_desk_bodies_v1") {
        let bodies = read.open_table(super::cache::DESK_BODIES);
        let mut taken = std::collections::BTreeSet::new();
        let order: BTreeMap<String, usize> = hosts
            .iter()
            .enumerate()
            .map(|(rank, (host, _))| (host.clone(), rank))
            .collect();
        let mut held: Vec<_> = bodies
            .iter()
            .map(|(key, value)| (key.value().into_owned(), value.value().into_owned()))
            .collect();
        held.sort_by_key(|(key, _)| order.get(&key.host).copied().unwrap_or(usize::MAX));
        for (key, body) in held {
            if !taken.insert(key.id.clone()) {
                continue;
            }
            let mut buffer = text::Buffer::new(
                text::ReplicaId::new(0),
                text::BufferId::new(1).expect("nonzero buffer id"),
                "",
            );
            buffer.apply_ops(
                body.operations
                    .iter()
                    .filter_map(|operation| operation.to_text().ok()),
            );
            nodes.entry(key.id).or_default().body = Some(buffer.text());
        }
    }
    nodes
}
