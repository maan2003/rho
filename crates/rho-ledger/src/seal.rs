//! Record framing and authenticated encryption; only devices use this module.
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::TryRng as _;

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
    rand::rngs::SysRng
        .try_fill_bytes(&mut nonce)
        .expect("system entropy");
    let body = XChaCha20Poly1305::new(key.into())
        .encrypt(
            &nonce.into(),
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
            &XNonce::try_from(nonce).ok()?,
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
    // Produced with chacha20poly1305 0.10.1 before the 0.11 update.
    #[test]
    fn opens_records_sealed_by_the_previous_crypto_version() {
        let key = std::array::from_fn(|i| (i * 7 + 3) as u8);
        let log = LogId(std::array::from_fn(|i| (i * 11 + 2) as u8));
        let at = 0x0102030405060708;
        let hex = concat!(
            "28010000010a131c252e374049525b646d767f88919aa3acb5bec7d0813862713c845c0233376bb0",
            "7f107e57df48e0bff9c9393447e46e206dd417a6fbb4860aaf4c36beac05859f3f6552edb5f1c698",
            "308a3ab456678803fee16da05a761bb32e95b92e2d4e55702f03b4b3374e01cb5f434862c58c89e5",
            "915f75dbe7acc050d6c2c988487567628ca846b27343fb4ea976d73a8496dc4221cbb5bc0bbc7bde",
            "8434ae30535a10a9d1baab33065f280425a7fcf6695ddd744be780c20b874779b0ac3a8f82cba9ae",
            "5b37e8a2e03b12b1cbae5248834d7f7552f9d73e40b5d6143bb7e099dac0b27c1759777c235b0d86",
            "39fb83f9d5a8f95a3a55ca23ba320dc65cf0737f53f6566fa74368b711ae1b85c6c937377db2c4c3",
            "c9cdc624b89f628c8afe080b5398c5bcdf474789",
        );
        let mut record: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(
            open(&key, log, at, &record),
            Some((300, b"old ledger\0asymmetric payload".to_vec()))
        );
        assert!(open(&key, log, at + 1, &record).is_none());
        record[73] ^= 1;
        assert!(open(&key, log, at, &record).is_none());
    }

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
