//! What the desk store holds, read off a copy.
//!
//! The rig proves what a client draws; this says what the daemon holds,
//! which is the other half of any "the GUI shows the wrong thing"
//! question, and the only way to see whether a write ever reached the
//! daemon at all. It opens a copy and refuses the live file:
//!
//!     cargo run -p rho-qa --example desk_cells -- \
//!         /home/maan2003/src/rho-rigs/snz/state/rho/rho.redb
//!
//! With no filter it prints the store's frontier and every thing that
//! carries a wake time. An argument filters to the ids it matches, and
//! prints their cells whether they are put away or not.

use redb::TableDefinition;
use rho_db::{RecordedTypeName, RhoDb, SenAs};
use rho_ui_proto::desk_tree::cells::{Cell, DeviceId, Id, Property, PropertyKey, Stamp, Version};
use senax_encoder::{Decode, Encode};

/// The daemon's own key and metadata shapes, redeclared: they are private
/// to `rho-daemon`, and senax encodes by field, so the same fields in the
/// same order read the same bytes. If the daemon's shapes change this
/// stops decoding, loudly, which is the right way for it to fail.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
struct CellAddress {
    id: Id,
    key: PropertyKey,
}

#[derive(Clone, Debug, Encode, Decode)]
struct CellMeta {
    daemon_device: DeviceId,
    frontier: Version,
    device_node_namespaces: Vec<(DeviceId, u16)>,
    next_node_namespace: u16,
}

/// The names the daemon's tables were written under. redb records the
/// Rust path of a value type and refuses a table that says another one,
/// so a reader outside `rho-daemon` has to answer to the daemon's names.
#[derive(Debug)]
struct AddressAsDaemonWroteIt;
#[derive(Debug)]
struct MetaAsDaemonWroteIt;
#[derive(Debug)]
struct CellAsDaemonWroteIt;

impl RecordedTypeName for AddressAsDaemonWroteIt {
    const NAME: &'static str = "rho-db::Sen<rho_daemon::desk_cells::CellAddress>";
}
impl RecordedTypeName for MetaAsDaemonWroteIt {
    const NAME: &'static str = "rho-db::Sen<rho_daemon::desk_cells::CellMeta>";
}
impl RecordedTypeName for CellAsDaemonWroteIt {
    const NAME: &'static str = "rho-db::Sen<rho_desk::cells::Cell>";
}

const CELLS: TableDefinition<
    SenAs<CellAddress, AddressAsDaemonWroteIt>,
    SenAs<Cell, CellAsDaemonWroteIt>,
> = TableDefinition::new("rho_desk_facts_v1");
const META: TableDefinition<(), SenAs<CellMeta, MetaAsDaemonWroteIt>> =
    TableDefinition::new("rho_desk_cell_meta_v2");

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: desk_cells <copy of rho.redb> [agent id]"))?;
    anyhow::ensure!(
        !path.contains("/.local/state/"),
        "that is the live store; take a snapshot and read the copy"
    );
    let wanted = args.next();

    let db = RhoDb::open(&path);
    let read = db.read();

    if let Some(meta) = read.open_table(META).get(&()) {
        let meta = meta.value().into_owned();
        println!("store {:?}", meta.daemon_device);
        println!("frontier:");
        for (device, counter) in &meta.frontier {
            let mine = if *device == meta.daemon_device {
                " (the daemon's own)"
            } else {
                ""
            };
            println!("  {device:?} {counter}{mine}");
        }
        println!("namespaces: {:?}", meta.device_node_namespaces);
    } else {
        println!("no metadata: this file has never held desk cells");
    }

    // Every cell of every agent the filter names, or of every agent that
    // carries a `DeferUntil` at all when it names none.
    let mut of_agent: std::collections::BTreeMap<String, Vec<(PropertyKey, Cell)>> =
        std::collections::BTreeMap::new();
    for (key, value) in read.open_table(CELLS).iter() {
        let key = key.value().into_owned();
        let name = format!("{:?}", key.id);
        if wanted
            .as_ref()
            .is_some_and(|wanted| !name.contains(wanted.as_str()))
        {
            continue;
        }
        let cell = value.value().into_owned();
        of_agent.entry(name).or_default().push((key.key, cell));
    }

    for (agent, cells) in &of_agent {
        let put_away = cells
            .iter()
            .any(|(_, cell)| matches!(&cell.property, Property::DeferUntil(Some(_))));
        if wanted.is_none() && !put_away {
            continue;
        }
        println!("\n{agent}{}", if put_away { "  PUT AWAY" } else { "" });
        let mut cells = cells.clone();
        cells.sort_by_key(|(key, _)| format!("{key:?}"));
        for (_, cell) in cells {
            println!("  {:?}  {}", cell.property, stamp(&cell.stamp));
        }
    }
    Ok(())
}

fn stamp(stamp: &Stamp) -> String {
    format!("stamp {:?}@{}", stamp.device, stamp.version)
}
