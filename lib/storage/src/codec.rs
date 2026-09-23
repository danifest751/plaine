use plaine_consensus::constants::{HEADER_BYTES, MAX_TXS_PER_BLOCK};

use crate::types::{Account, HeaderStatus, SideHeader, UndoRec};

pub const STATE_VALUE_BYTES: usize = 24;

pub fn encode_account(a: &Account) -> [u8; STATE_VALUE_BYTES] {
    let mut v = [0u8; STATE_VALUE_BYTES];
    v[0..16].copy_from_slice(&a.balance.to_le_bytes());
    v[16..24].copy_from_slice(&a.nonce.to_le_bytes());
    v
}

pub fn decode_account(v: &[u8; STATE_VALUE_BYTES]) -> Account {
    let mut b = [0u8; 16];
    b.copy_from_slice(&v[0..16]);
    let mut n = [0u8; 8];
    n.copy_from_slice(&v[16..24]);
    Account {
        balance: u128::from_le_bytes(b),
        nonce: u64::from_le_bytes(n),
    }
}

pub const UNDO_REC_BYTES: usize = 45;

pub const MAX_UNDO_RECS: usize = 2 * MAX_TXS_PER_BLOCK + 1;

pub const UNDO_HDR_BYTES: usize = 20;
pub const MAX_UNDO_BLOB_BYTES: usize = UNDO_HDR_BYTES + MAX_UNDO_RECS * UNDO_REC_BYTES;

pub fn encode_undo(recs: &[UndoRec], issued_delta: u128, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(UNDO_HDR_BYTES + recs.len() * UNDO_REC_BYTES);
    out.extend_from_slice(&(recs.len() as u32).to_le_bytes());
    out.extend_from_slice(&issued_delta.to_le_bytes());
    for r in recs {
        out.extend_from_slice(&r.addr);
        out.extend_from_slice(&r.prev_balance.to_le_bytes());
        out.extend_from_slice(&r.prev_nonce.to_le_bytes());

        // inverted flag: 0 == existed. the common case (account already there)
        // then encodes as a zero byte.
        out.push(if r.existed { 0 } else { 1 });
    }
}

pub fn decode_undo(blob: &[u8]) -> Option<(Vec<UndoRec>, u128)> {
    if blob.len() < UNDO_HDR_BYTES {
        return None;
    }
    let count = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;

    if count > MAX_UNDO_RECS || blob.len() != UNDO_HDR_BYTES + count * UNDO_REC_BYTES {
        return None;
    }
    let mut ib = [0u8; 16];
    ib.copy_from_slice(&blob[4..20]);
    let issued = u128::from_le_bytes(ib);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let o = UNDO_HDR_BYTES + i * UNDO_REC_BYTES;
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&blob[o..o + 20]);
        let mut b = [0u8; 16];
        b.copy_from_slice(&blob[o + 20..o + 36]);
        let mut n = [0u8; 8];
        n.copy_from_slice(&blob[o + 36..o + 44]);
        out.push(UndoRec {
            addr,
            prev_balance: u128::from_le_bytes(b),
            prev_nonce: u64::from_le_bytes(n),
            existed: blob[o + 44] & 1 == 0,
        });
    }
    Some((out, issued))
}

pub const SIDE_VALUE_BYTES: usize = HEADER_BYTES + 9;

pub fn encode_side(h: &[u8; HEADER_BYTES], height: u64, st: HeaderStatus) -> [u8; SIDE_VALUE_BYTES] {
    let mut v = [0u8; SIDE_VALUE_BYTES];
    v[0..HEADER_BYTES].copy_from_slice(h);
    v[HEADER_BYTES..HEADER_BYTES + 8].copy_from_slice(&height.to_le_bytes());
    v[HEADER_BYTES + 8] = st as u8;
    v
}

pub fn decode_side(v: &[u8; SIDE_VALUE_BYTES]) -> Option<SideHeader> {
    let mut header = [0u8; HEADER_BYTES];
    header.copy_from_slice(&v[0..HEADER_BYTES]);
    let mut hb = [0u8; 8];
    hb.copy_from_slice(&v[HEADER_BYTES..HEADER_BYTES + 8]);
    Some(SideHeader {
        header,
        height: u64::from_le_bytes(hb),
        status: HeaderStatus::from_code(v[HEADER_BYTES + 8])?,
    })
}

pub fn side_by_height_key(height: u64, hash: &[u8; 32]) -> [u8; 40] {
    let mut k = [0u8; 40];

    k[0..8].copy_from_slice(&height.to_be_bytes());
    k[8..40].copy_from_slice(hash);
    k
}

#[inline]
pub fn hash_prefix_n(h: &[u8; 32], n: u32) -> u64 {
    let n = n.clamp(1, 8) as usize;
    let mut v = 0u64;
    for b in &h[..n] {
        v = (v << 8) | *b as u64;
    }
    v
}

pub fn txid_key(txid: &[u8; 32]) -> [u8; 16] {
    let mut k = [0u8; 16];
    k.copy_from_slice(&txid[0..16]);
    k
}

pub const ADDR_KEY_BYTES: usize = 30;

pub fn addr_key(addr: &[u8; 20], height: u64, index: u16) -> [u8; ADDR_KEY_BYTES] {
    let mut k = [0u8; ADDR_KEY_BYTES];
    k[0..20].copy_from_slice(addr);
    k[20..28].copy_from_slice(&height.to_be_bytes());
    k[28..30].copy_from_slice(&index.to_be_bytes());
    k
}

pub fn decode_addr_key(k: &[u8; ADDR_KEY_BYTES]) -> ([u8; 20], u64, u16) {
    let mut a = [0u8; 20];
    a.copy_from_slice(&k[0..20]);
    let mut h = [0u8; 8];
    h.copy_from_slice(&k[20..28]);
    (a, u64::from_be_bytes(h), u16::from_be_bytes([k[28], k[29]]))
}

pub fn encode_txloc(height: u64, index: u16) -> [u8; 10] {
    let mut v = [0u8; 10];
    v[0..8].copy_from_slice(&height.to_le_bytes());
    v[8..10].copy_from_slice(&index.to_le_bytes());
    v
}

pub fn decode_txloc(v: &[u8; 10]) -> (u64, u16) {
    let mut h = [0u8; 8];
    h.copy_from_slice(&v[0..8]);
    (u64::from_le_bytes(h), u16::from_le_bytes([v[8], v[9]]))
}

pub fn encode_frame_header(len: u32, crc: u32) -> [u8; 8] {
    let mut v = [0u8; 8];
    v[0..4].copy_from_slice(&len.to_le_bytes());
    v[4..8].copy_from_slice(&crc.to_le_bytes());
    v
}

pub fn decode_frame_header(v: &[u8; 8]) -> (u32, u32) {
    (
        u32::from_le_bytes([v[0], v[1], v[2], v[3]]),
        u32::from_le_bytes([v[4], v[5], v[6], v[7]]),
    )
}

pub fn encode_bidx(offset: u32, len: u32) -> [u8; 8] {
    let mut v = [0u8; 8];
    v[0..4].copy_from_slice(&offset.to_le_bytes());
    v[4..8].copy_from_slice(&len.to_le_bytes());
    v
}

pub fn decode_bidx(v: &[u8; 8]) -> (u32, u32) {
    (
        u32::from_le_bytes([v[0], v[1], v[2], v[3]]),
        u32::from_le_bytes([v[4], v[5], v[6], v[7]]),
    )
}

pub fn row_digest(addr: &[u8; 20], a: &Account) -> [u8; 32] {
    let mut buf = [0u8; 44];
    buf[0..20].copy_from_slice(addr);
    buf[20..44].copy_from_slice(&encode_account(a));
    plaine_consensus::blake3::hash(&buf)
}

// The state fingerprint is a running XOR of per-row digests. XOR commutes, so the
// result doesn't care what order rows were applied or rolled back in - which is
// exactly what lets a reorg leave the fingerprint self-consistent.
pub fn fp_xor(fp: &mut [u8; 32], d: &[u8; 32]) {
    for i in 0..32 {
        fp[i] ^= d[i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_roundtrip_and_bounds() {
        let recs = vec![
            UndoRec { addr: [1u8; 20], prev_balance: 7, prev_nonce: 3, existed: true },
            UndoRec { addr: [2u8; 20], prev_balance: 0, prev_nonce: 0, existed: false },
        ];
        let mut blob = Vec::new();
        encode_undo(&recs, 99, &mut blob);
        assert_eq!(blob.len(), UNDO_HDR_BYTES + 2 * UNDO_REC_BYTES);
        assert_eq!(decode_undo(&blob).unwrap(), (recs, 99u128));

        let mut bad = blob.clone();
        bad[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_undo(&bad).is_none());
    }

    #[test]
    fn account_roundtrip() {
        let a = Account { balance: u128::MAX - 5, nonce: 1234 };
        assert_eq!(decode_account(&encode_account(&a)), a);
    }

    #[test]
    fn fingerprint_is_order_independent() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        let rows = [
            ([1u8; 20], Account { balance: 5, nonce: 1 }),
            ([2u8; 20], Account { balance: 9, nonce: 2 }),
            ([3u8; 20], Account { balance: 0, nonce: 7 }),
        ];
        for (addr, acct) in rows.iter() {
            fp_xor(&mut a, &row_digest(addr, acct));
        }
        for (addr, acct) in rows.iter().rev() {
            fp_xor(&mut b, &row_digest(addr, acct));
        }
        assert_eq!(a, b);
    }
}
