//! Sealing segments with the key the user's devices share.
//!
//! XChaCha20-Poly1305 with a random nonce per segment. The device, the
//! segment's number and whether it is a base are bound in as associated
//! data, so a host cannot pass one device's segment off as another's, or
//! an old segment off as a newer one.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore as _;

use crate::entry::Entry;
use crate::protocol::DeviceId;

const NONCE_LEN: usize = 24;

fn associated(device: DeviceId, seq: u64, base: bool) -> Vec<u8> {
    let mut data = b"rho-ledger/1".to_vec();
    data.extend_from_slice(&device.0);
    data.extend_from_slice(&seq.to_le_bytes());
    data.push(u8::from(base));
    data
}

pub(crate) fn seal(
    key: &[u8; 32],
    device: DeviceId,
    seq: u64,
    base: bool,
    entries: &[Entry],
) -> Vec<u8> {
    let plain = senax_encoder::encode(&entries.to_vec()).expect("encode ledger entries");
    let mut nonce = [0; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let aad = associated(device, seq, base);
    let sealed = XChaCha20Poly1305::new(key.into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &plain,
                aad: &aad,
            },
        )
        .expect("seal a ledger segment");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    out
}

/// The entries in a segment, or `None` when it does not open with this
/// key as this device's segment `seq`: the wrong key, or a segment that
/// was tampered with or moved.
pub(crate) fn open(
    key: &[u8; 32],
    device: DeviceId,
    seq: u64,
    base: bool,
    sealed: &[u8],
) -> Option<Vec<Entry>> {
    if sealed.len() < NONCE_LEN {
        return None;
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let aad = associated(device, seq, base);
    let plain = XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: &aad,
            },
        )
        .ok()?;
    let mut plain: &[u8] = &plain;
    senax_encoder::decode(&mut plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Stamp;

    const DEVICE: DeviceId = DeviceId([5; 16]);
    const KEY: [u8; 32] = [9; 32];

    fn entries() -> Vec<Entry> {
        vec![Entry {
            key: b"k".to_vec(),
            value: Some(b"v".to_vec()),
            stamp: Stamp {
                millis: 1,
                counter: 0,
                device: DEVICE,
            },
        }]
    }

    #[test]
    fn a_sealed_segment_opens_with_its_key_where_it_was_put() {
        let sealed = seal(&KEY, DEVICE, 3, false, &entries());
        assert_eq!(open(&KEY, DEVICE, 3, false, &sealed), Some(entries()));
    }

    #[test]
    fn a_segment_does_not_open_with_another_key_or_somewhere_else() {
        let sealed = seal(&KEY, DEVICE, 3, false, &entries());
        assert_eq!(open(&[8; 32], DEVICE, 3, false, &sealed), None);
        assert_eq!(open(&KEY, DeviceId([6; 16]), 3, false, &sealed), None);
        assert_eq!(open(&KEY, DEVICE, 4, false, &sealed), None);
        assert_eq!(open(&KEY, DEVICE, 3, true, &sealed), None);
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(open(&KEY, DEVICE, 3, false, &tampered), None);
    }
}
