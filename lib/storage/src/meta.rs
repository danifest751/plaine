use crate::error::StoreError;
use crate::tables::META;
use crate::types::TipRef;

pub const K_SCHEMA: &str = "schema_version";
pub const K_NETWORK: &str = "network";
pub const K_TIP: &str = "tip";
pub const K_HDR_WM: &str = "hdr_watermark";
pub const K_BODY_WM: &str = "body_watermark";
pub const K_PRUNE_FLOOR: &str = "prune_floor";
pub const K_UNDO_FLOOR: &str = "undo_floor";
pub const K_ISSUED: &str = "issued";
pub const K_FINGERPRINT: &str = "state_fingerprint";
pub const K_INDEX_STATE: &str = "index_state";
pub const K_STALE_INDEX: &str = "stale_index_count";
pub const K_TXINDEX_FROM: &str = "txindex_from";
pub const K_ADDRINDEX_FROM: &str = "addrindex_from";
pub const K_IBD_BATCH: &str = "ibd_batch_blocks";
pub const K_BIDX_SEALED: &str = "bidx_sealed_through";
pub const K_FULLIDX_ROWS: &str = "hash_index_full_rows";
pub const K_INVALID_SEQ: &str = "invalid_next_seq";
pub const K_INVALID_ROWS: &str = "invalid_rows";
pub const K_SIDE_ROWS: &str = "side_rows";
pub const K_BODY_APPEND: &str = "body_append_offset";
pub const K_ANCHOR_FLOOR: &str = "anchor_floor";

pub const K_ANCHOR_CP: &str = "checkpoint_anchor";

#[derive(Debug, Clone, Copy)]
pub struct Meta {
    pub schema_version: u32,
    pub network: [u8; 4],
    pub tip: TipRef,
    pub hdr_watermark: u64,
    pub body_watermark: u64,
    pub prune_floor: u64,
    pub undo_floor: u64,
    pub issued: u128,
    pub fingerprint: [u8; 32],
    pub index_state: u8,
    pub stale_index_count: u64,
    pub txindex_from: Option<u64>,
    pub addrindex_from: Option<u64>,
    pub ibd_batch_blocks: u32,
    pub bidx_sealed_through: i64,
    pub hash_index_full_rows: u64,
    pub invalid_next_seq: u64,
    pub invalid_rows: u64,
    pub side_rows: u64,
    pub body_append_offset: u32,
    pub anchor_floor: Option<u64>,
}

impl Meta {
    pub fn fresh(network: [u8; 4], prune_floor: u64, ibd_batch_blocks: u32) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION,
            network,
            tip: TipRef::default(),
            hdr_watermark: 0,
            body_watermark: 0,
            prune_floor,
            undo_floor: 0,
            issued: 0,
            fingerprint: [0u8; 32],
            index_state: 0,
            stale_index_count: 0,
            txindex_from: None,
            addrindex_from: None,
            ibd_batch_blocks,
            bidx_sealed_through: -1,
            hash_index_full_rows: 0,
            invalid_next_seq: 0,
            invalid_rows: 0,
            side_rows: 0,
            body_append_offset: 0,
            anchor_floor: None,
        }
    }
}

fn u64_of(b: &[u8]) -> u64 {
    let mut x = [0u8; 8];
    let n = b.len().min(8);
    x[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(x)
}

fn fixed<'a>(k: &'static str, v: &'a [u8], want: usize) -> Result<&'a [u8], StoreError> {
    if v.len() != want {
        return Err(StoreError::MetaRowMalformed {
            key: k,
            len: v.len(),
            expected: want,
        });
    }
    Ok(v)
}

pub fn schema_of(db: &redb::Database) -> Result<Option<u32>, StoreError> {
    let txn = db.begin_read()?;
    let t = match txn.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(t.get(K_SCHEMA)?.map(|v| u64_of(v.value()) as u32))
}

pub fn load(db: &redb::Database) -> Result<Option<Meta>, StoreError> {
    let txn = db.begin_read()?;
    let t = match txn.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let get = |k: &str| -> Result<Option<Vec<u8>>, StoreError> {
        Ok(t.get(k)?.map(|v| v.value().to_vec()))
    };
    let Some(schema) = get(K_SCHEMA)? else { return Ok(None) };
    let mut network = [0u8; 4];
    if let Some(n) = get(K_NETWORK)? {
        network.copy_from_slice(fixed(K_NETWORK, &n, 4)?);
    }
    let mut tip = TipRef::default();
    if let Some(v) = get(K_TIP)? {
        let v = fixed(K_TIP, &v, 72)?;
        tip.hash.copy_from_slice(&v[0..32]);
        tip.height = u64_of(&v[32..40]);
        tip.chainwork.copy_from_slice(&v[40..72]);
    }
    let mut fingerprint = [0u8; 32];
    if let Some(v) = get(K_FINGERPRINT)? {
        fingerprint.copy_from_slice(fixed(K_FINGERPRINT, &v, 32)?);
    }
    let issued = match get(K_ISSUED)? {
        Some(v) => {
            let mut x = [0u8; 16];
            x.copy_from_slice(fixed(K_ISSUED, &v, 16)?);
            u128::from_le_bytes(x)
        }
        None => 0,
    };
    let index_state = match get(K_INDEX_STATE)? {
        Some(v) => fixed(K_INDEX_STATE, &v, 1)?[0],
        None => 0,
    };

    let bidx_sealed_through = match get(K_BIDX_SEALED)? {
        Some(v) => {
            let raw = u64_of(fixed(K_BIDX_SEALED, &v, 8)?);
            if raw > u32::MAX as u64 + 1 {
                return Err(StoreError::MetaRowMalformed {
                    key: K_BIDX_SEALED,
                    len: raw as usize,
                    expected: u32::MAX as usize + 1,
                });
            }
            // written as value+1, so the "nothing sealed yet" sentinel of -1
            // survives a round trip through an unsigned meta row as plain 0.
            raw as i64 - 1
        }
        None => -1,
    };
    Ok(Some(Meta {
        schema_version: u64_of(&schema) as u32,
        network,
        tip,
        hdr_watermark: get(K_HDR_WM)?.map(|v| u64_of(&v)).unwrap_or(0),
        body_watermark: get(K_BODY_WM)?.map(|v| u64_of(&v)).unwrap_or(0),
        prune_floor: get(K_PRUNE_FLOOR)?.map(|v| u64_of(&v)).unwrap_or(0),
        undo_floor: get(K_UNDO_FLOOR)?.map(|v| u64_of(&v)).unwrap_or(0),
        issued,
        fingerprint,
        index_state,
        stale_index_count: get(K_STALE_INDEX)?.map(|v| u64_of(&v)).unwrap_or(0),
        txindex_from: get(K_TXINDEX_FROM)?.map(|v| u64_of(&v)),
        addrindex_from: get(K_ADDRINDEX_FROM)?.map(|v| u64_of(&v)),
        ibd_batch_blocks: get(K_IBD_BATCH)?.map(|v| u64_of(&v) as u32).unwrap_or(8_192),
        bidx_sealed_through,
        hash_index_full_rows: get(K_FULLIDX_ROWS)?.map(|v| u64_of(&v)).unwrap_or(0),
        invalid_next_seq: get(K_INVALID_SEQ)?.map(|v| u64_of(&v)).unwrap_or(0),
        invalid_rows: get(K_INVALID_ROWS)?.map(|v| u64_of(&v)).unwrap_or(0),
        side_rows: get(K_SIDE_ROWS)?.map(|v| u64_of(&v)).unwrap_or(0),
        body_append_offset: get(K_BODY_APPEND)?.map(|v| u64_of(&v) as u32).unwrap_or(0),
        anchor_floor: get(K_ANCHOR_FLOOR)?.map(|v| u64_of(&v)),
    }))
}

pub fn store(txn: &redb::WriteTransaction, m: &Meta) -> Result<(), StoreError> {
    let mut t = txn.open_table(META)?;
    let mut put = |k: &str, v: &[u8]| -> Result<(), StoreError> {
        t.insert(k, v)?;
        Ok(())
    };
    put(K_SCHEMA, &(m.schema_version as u64).to_le_bytes())?;
    put(K_NETWORK, &m.network)?;
    let mut tip = [0u8; 72];
    tip[0..32].copy_from_slice(&m.tip.hash);
    tip[32..40].copy_from_slice(&m.tip.height.to_le_bytes());
    tip[40..72].copy_from_slice(&m.tip.chainwork);
    put(K_TIP, &tip)?;
    put(K_HDR_WM, &m.hdr_watermark.to_le_bytes())?;
    put(K_BODY_WM, &m.body_watermark.to_le_bytes())?;
    put(K_PRUNE_FLOOR, &m.prune_floor.to_le_bytes())?;
    put(K_UNDO_FLOOR, &m.undo_floor.to_le_bytes())?;
    put(K_ISSUED, &m.issued.to_le_bytes())?;
    put(K_FINGERPRINT, &m.fingerprint)?;
    put(K_INDEX_STATE, &[m.index_state])?;
    put(K_STALE_INDEX, &m.stale_index_count.to_le_bytes())?;
    if let Some(f) = m.txindex_from {
        put(K_TXINDEX_FROM, &f.to_le_bytes())?;
    }
    if let Some(f) = m.addrindex_from {
        put(K_ADDRINDEX_FROM, &f.to_le_bytes())?;
    }
    put(K_IBD_BATCH, &(m.ibd_batch_blocks as u64).to_le_bytes())?;
    put(K_BIDX_SEALED, &((m.bidx_sealed_through + 1) as u64).to_le_bytes())?;
    put(K_FULLIDX_ROWS, &m.hash_index_full_rows.to_le_bytes())?;
    put(K_INVALID_SEQ, &m.invalid_next_seq.to_le_bytes())?;
    put(K_INVALID_ROWS, &m.invalid_rows.to_le_bytes())?;
    put(K_SIDE_ROWS, &m.side_rows.to_le_bytes())?;
    put(K_BODY_APPEND, &(m.body_append_offset as u64).to_le_bytes())?;

    if let Some(f) = m.anchor_floor {
        put(K_ANCHOR_FLOOR, &f.to_le_bytes())?;
    }
    Ok(())
}
