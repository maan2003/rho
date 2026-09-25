//! One sealed blob per note slot. A put replaces a slot's blob only if it
//! names the blob held; every accepted put takes the next version, so a
//! device asks for what changed since the last version it saw.
use redb::TableDefinition;
use rho_db::{ReadTxn, WriteTxn};
use rho_ledger::protocol::{BlobHash, SlotId, StoreId};

const STORE: TableDefinition<(), [u8; 16]> = TableDefinition::new("ledger_slot_store_v1");
/// Slot → (version, blob).
const SLOTS: TableDefinition<[u8; 16], (u64, &[u8])> = TableDefinition::new("ledger_slots_v1");
/// Version → the slot it is the latest version of.
const VERSIONS: TableDefinition<u64, [u8; 16]> = TableDefinition::new("ledger_slot_versions_v1");

pub(crate) fn open(write: &mut WriteTxn) -> StoreId {
    write.open_table(SLOTS);
    write.open_table(VERSIONS);
    let mut store = write.open_table(STORE);
    if let Some(id) = store.get(()) {
        return StoreId(id.value());
    }
    let mut id = [0; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut id);
    store.insert((), id);
    StoreId(id)
}

pub(crate) fn hash(blob: &[u8]) -> BlobHash {
    *blake3::hash(blob).as_bytes()
}

/// The slot's new version, or what it still holds if `prev` is not it:
/// version 0 and no bytes for a slot never put.
pub(crate) fn put(
    write: &mut WriteTxn,
    slot: SlotId,
    prev: Option<BlobHash>,
    blob: &[u8],
) -> Result<u64, (u64, Vec<u8>)> {
    let held = write.open_table(SLOTS).get(slot.0).map(|held| {
        let (version, blob) = held.value();
        (version, blob.to_vec())
    });
    if held.as_ref().map(|(_, blob)| hash(blob)) != prev {
        return Err(held.unwrap_or_default());
    }
    let mut versions = write.open_table(VERSIONS);
    let version = versions
        .iter()
        .next_back()
        .map_or(0, |(last, _)| last.value())
        + 1;
    if let Some((old, _)) = held {
        versions.remove(old);
    }
    versions.insert(version, slot.0);
    drop(versions);
    write.open_table(SLOTS).insert(slot.0, (version, blob));
    Ok(version)
}

/// The first slot changed after `since`, with its version and blob.
pub(crate) fn next_after(read: &ReadTxn, since: u64) -> Option<(SlotId, u64, Vec<u8>)> {
    let (version, slot) = read
        .open_table(VERSIONS)
        .range((since + 1)..)
        .next()
        .map(|(version, slot)| (version.value(), slot.value()))?;
    let blob = read
        .open_table(SLOTS)
        .get(slot)
        .map(|held| held.value().1.to_vec())
        .expect("a version names a held slot");
    Some((SlotId(slot), version, blob))
}
