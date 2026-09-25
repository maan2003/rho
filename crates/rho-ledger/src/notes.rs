//! Notes, one sealed blob per note in a slot on each host. A device holds
//! each note's newest revision as it last chose it; a host holds whatever
//! blob was last put, and replaces it only for a put that names it.
use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_db::{Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use crate::ledger::Ledger;
use crate::protocol::{BlobHash, LogId, SlotId, StoreId};
use crate::seal;
use crate::secret::Secret;

const SLOT_CONTEXT: &str = "rho 2026-09-25 note slot";
const SEAL_CONTEXT: &str = "rho 2026-09-25 note";

/// Note → what this device holds of it.
const NOTES: TableDefinition<[u8; 16], Sen<Held>> = TableDefinition::new("ledger_notes_v1");
/// (store, slot) → the blob that host was last seen holding.
const HOSTS: TableDefinition<([u8; 16], [u8; 16]), BlobHash> =
    TableDefinition::new("ledger_note_hosts_v1");
/// Store → the last slot version seen from it.
const SEEN: TableDefinition<[u8; 16], u64> = TableDefinition::new("ledger_note_seen_v1");

#[derive(Clone, Debug, Encode, Decode)]
struct Held {
    plain: Vec<u8>,
    /// Sealed once the secret is here, and kept as sealed so every host is
    /// put the same bytes.
    blob: Option<Vec<u8>>,
    /// Written here and not yet seen held by any host.
    unsent: bool,
}
#[derive(Clone, Debug, Encode, Decode)]
struct Sealed {
    note: [u8; 16],
    plain: Vec<u8>,
}

/// A note another device wrote that this device now holds instead of its
/// own.
#[derive(Debug, PartialEq, Eq)]
pub struct Arrived {
    pub plain: Vec<u8>,
    /// What this device held, if it held the note.
    pub replaced: Option<Vec<u8>>,
    /// Whether what it held had never reached a host.
    pub replaced_unsent: bool,
}

pub(crate) fn open(write: &mut WriteTxn) {
    write.open_table(NOTES);
    write.open_table(HOSTS);
    write.open_table(SEEN);
}

fn slot_id(secret: Secret, note: [u8; 16]) -> SlotId {
    let hash = blake3::keyed_hash(&secret.derive(SLOT_CONTEXT), &note);
    SlotId(hash.as_bytes()[..16].try_into().unwrap())
}

fn hash(blob: &[u8]) -> BlobHash {
    *blake3::hash(blob).as_bytes()
}

fn seal_note(secret: Secret, note: [u8; 16], plain: &[u8]) -> Vec<u8> {
    let sealed = senax_encoder::encode(&Sealed {
        note,
        plain: plain.to_vec(),
    })
    .expect("encode note");
    seal::seal(
        &secret.derive(SEAL_CONTEXT),
        LogId(slot_id(secret, note).0),
        0,
        &sealed,
    )
}

fn open_note(secret: Secret, slot: SlotId, blob: &[u8]) -> Option<Sealed> {
    let (size, plain) = seal::open(&secret.derive(SEAL_CONTEXT), LogId(slot.0), 0, blob)?;
    let sealed = senax_encoder::decode::<Sealed>(&mut plain.as_slice()).ok()?;
    // A host may only move blobs between slots, and a blob names its slot.
    (size == blob.len() && slot_id(secret, sealed.note) == slot).then_some(sealed)
}

/// Seals what was written before the secret arrived.
pub(crate) fn seal_held(write: &mut WriteTxn, secret: Secret) {
    let unsealed: Vec<([u8; 16], Held)> = write
        .open_table(NOTES)
        .iter()
        .map(|(note, held)| (note.value(), held.value().into_owned()))
        .filter(|(_, held)| held.blob.is_none())
        .collect();
    let mut notes = write.open_table(NOTES);
    for (note, mut held) in unsealed {
        held.blob = Some(seal_note(secret, note, &held.plain));
        notes.insert(note, SenValue::borrowed(&held));
    }
}

impl Ledger {
    /// Every note this device holds.
    pub fn notes(&self) -> Vec<Vec<u8>> {
        self.db()
            .read()
            .open_table(NOTES)
            .iter()
            .map(|(_, held)| held.value().into_owned().plain)
            .collect()
    }

    pub async fn put_note(&self, note: [u8; 16], plain: Vec<u8>) {
        let secret = self.secret();
        let mut write = self.db().write().await;
        let blob = secret.map(|secret| seal_note(secret, note, &plain));
        write.open_table(NOTES).insert(
            note,
            SenValue::borrowed(&Held {
                plain,
                blob,
                unsent: true,
            }),
        );
        write.commit();
    }

    /// How far this device has seen each host's slots.
    pub fn slots_seen(&self) -> BTreeMap<StoreId, u64> {
        self.db()
            .read()
            .open_table(SEEN)
            .iter()
            .map(|(store, version)| (StoreId(store.value()), version.value()))
            .collect()
    }

    /// The puts that would bring `store` to what this device holds.
    pub fn note_puts(&self, store: StoreId) -> Vec<(SlotId, Option<BlobHash>, Vec<u8>)> {
        let Some(secret) = self.secret() else {
            return Vec::new();
        };
        let read = self.db().read();
        let hosts = read.open_table(HOSTS);
        read.open_table(NOTES)
            .iter()
            .filter_map(|(note, held)| {
                let blob = held.value().into_owned().blob?;
                let slot = slot_id(secret, note.value());
                let prev = hosts.get((store.0, slot.0)).map(|held| held.value());
                (prev != Some(hash(&blob))).then_some((slot, prev, blob))
            })
            .collect()
    }

    /// What `store` holds in each slot, as of each version, in one write.
    /// `keep_theirs` says whether their note replaces this device's; the
    /// answer is what arrived.
    pub async fn receive_slots(
        &self,
        store: StoreId,
        slots: Vec<(SlotId, u64, Vec<u8>)>,
        keep_theirs: &(dyn Fn(&[u8], &[u8]) -> bool + Send + Sync),
    ) -> Vec<Arrived> {
        let Some(secret) = self.secret() else {
            return Vec::new();
        };
        let mut write = self.db().write().await;
        let arrived = slots
            .into_iter()
            .filter_map(|(slot, version, blob)| {
                receive_slot(&mut write, secret, store, slot, version, blob, keep_theirs)
            })
            .collect();
        write.commit();
        arrived
    }
}

fn receive_slot(
    write: &mut WriteTxn,
    secret: Secret,
    store: StoreId,
    slot: SlotId,
    version: u64,
    blob: Vec<u8>,
    keep_theirs: &(dyn Fn(&[u8], &[u8]) -> bool + Send + Sync),
) -> Option<Arrived> {
    let mut seen = write.open_table(SEEN);
    let last = seen.get(store.0).map_or(0, |seen| seen.value());
    seen.insert(store.0, last.max(version));
    drop(seen);
    if blob.is_empty() {
        write.open_table(HOSTS).remove((store.0, slot.0));
        return None;
    }
    write
        .open_table(HOSTS)
        .insert((store.0, slot.0), hash(&blob));
    let theirs = open_note(secret, slot, &blob)?;
    let mine = write
        .open_table(NOTES)
        .get(theirs.note)
        .map(|held| held.value().into_owned());
    match mine {
        Some(mut mine) if mine.blob.as_deref() == Some(blob.as_slice()) => {
            mine.unsent = false;
            write
                .open_table(NOTES)
                .insert(theirs.note, SenValue::borrowed(&mine));
            None
        }
        Some(mine) if !keep_theirs(&theirs.plain, &mine.plain) => None,
        mine => {
            write.open_table(NOTES).insert(
                theirs.note,
                SenValue::borrowed(&Held {
                    plain: theirs.plain.clone(),
                    blob: Some(blob),
                    unsent: false,
                }),
            );
            Some(Arrived {
                plain: theirs.plain,
                replaced_unsent: mine.as_ref().is_some_and(|mine| mine.unsent),
                replaced: mine.map(|mine| mine.plain),
            })
        }
    }
}
