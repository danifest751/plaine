use redb::TableDefinition;

pub const STATE: TableDefinition<&[u8; 20], &[u8; 24]> = TableDefinition::new("state");

pub const UNDO: TableDefinition<u64, &[u8]> = TableDefinition::new("undo");

// primary hash->height index keyed by a short hash prefix to keep it small; the
// full-hash table below only holds the rare entries whose prefix already collided.
pub const HASH_INDEX: TableDefinition<u64, u64> = TableDefinition::new("hash_index");

pub const HASH_INDEX_FULL: TableDefinition<&[u8; 32], u64> =
    TableDefinition::new("hash_index_full");

pub const SIDE_HEADERS: TableDefinition<&[u8; 32], &[u8; 141]> =
    TableDefinition::new("side_headers");

pub const SIDE_BY_HEIGHT: TableDefinition<&[u8; 40], ()> = TableDefinition::new("side_by_height");

pub const INVALID: TableDefinition<&[u8; 32], u8> = TableDefinition::new("invalid");

pub const INVALID_SEQ: TableDefinition<u64, &[u8; 32]> = TableDefinition::new("invalid_seq");

pub const CHAINWORK_CKPT: TableDefinition<u64, &[u8; 32]> = TableDefinition::new("chainwork_ckpt");

pub const TXINDEX: TableDefinition<&[u8; 16], &[u8; 10]> = TableDefinition::new("txindex");

// address (20) || height (8, big-endian) || tx index (2, big-endian). Big-endian so
// one address's rows sort by position in the chain and a reverse range walk yields
// its history newest first. Like TXINDEX, rows are never deleted on a reorg: a
// reader checks each hit against the canonical body at that height.
pub const ADDRINDEX: TableDefinition<&[u8; 30], ()> = TableDefinition::new("addrindex");

pub const HDR_UNDO: TableDefinition<u64, &[u8]> = TableDefinition::new("hdr_undo");

pub const STATE_CKPT: TableDefinition<u64, u64> = TableDefinition::new("state_ckpt");

pub const BODY_ANCHOR: TableDefinition<u32, &[u8; 65]> = TableDefinition::new("body_anchor");

pub const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
