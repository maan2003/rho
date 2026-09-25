//! Record framing and authenticated encryption; only devices use this module.
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore as _;

use crate::protocol::LogId;

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
fn associated(log: LogId, at: u64) -> Vec<u8> {
    let mut aad = b"rho-ledger/log/1".to_vec();
    aad.extend_from_slice(&log.0);
    aad.extend_from_slice(&at.to_le_bytes());
    aad
}

pub(crate) fn seal(key: &[u8; 32], log: LogId, at: u64, payload: &[u8]) -> Vec<u8> {
    let real_len = u32::try_from(payload.len()).expect("ledger payload fits u32");
    let padded = (payload.len() + 4).max(256).next_power_of_two();
    let mut plain = vec![0; padded];
    plain[..4].copy_from_slice(&real_len.to_le_bytes());
    plain[4..4 + payload.len()].copy_from_slice(payload);
    let mut nonce = [0; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let body = XChaCha20Poly1305::new(key.into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &plain,
                aad: &associated(log, at),
            },
        )
        .expect("seal ledger record");
    let len = u32::try_from(NONCE_LEN + body.len()).expect("ledger record fits u32");
    let mut record = Vec::with_capacity(4 + len as usize);
    record.extend_from_slice(&len.to_le_bytes());
    record.extend_from_slice(&nonce);
    record.extend_from_slice(&body);
    record
}

/// Returns (record length, plaintext), or None if incomplete or invalid.
pub(crate) fn open(key: &[u8; 32], log: LogId, at: u64, record: &[u8]) -> Option<(usize, Vec<u8>)> {
    let len = usize::try_from(u32::from_le_bytes(record.get(..4)?.try_into().ok()?)).ok()?;
    if len < NONCE_LEN + TAG_LEN + 256 || len.checked_add(4)? > record.len() {
        return None;
    }
    let (nonce, body) = record[4..4 + len].split_at(NONCE_LEN);
    let plain = XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: &associated(log, at),
            },
        )
        .ok()?;
    let real = u32::from_le_bytes(plain.get(..4)?.try_into().ok()?) as usize;
    if real > plain.len() - 4 || !plain[4 + real..].iter().all(|byte| *byte == 0) {
        return None;
    }
    Some((4 + len, plain[4..4 + real].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padding_and_binding() {
        let key = [1; 32];
        let log = LogId([2; 16]);
        for (size, padded) in [(0, 256), (252, 256), (253, 512), (600, 1024)] {
            let bytes = vec![7; size];
            let record = seal(&key, log, 37, &bytes);
            assert_eq!(record.len(), 4 + 24 + padded + 16);
            assert_eq!(open(&key, log, 37, &record), Some((record.len(), bytes)));
            assert!(open(&key, log, 38, &record).is_none());
            assert!(open(&key, LogId([3; 16]), 37, &record).is_none());
        }
    }
}
