use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InvalidReason {
    BadPow = 1,
    BadBits = 2,
    BadMtp = 3,
    BadMerkle = 4,
    BadTx = 5,
    BadParent = 6,
    BadCheckpoint = 7,
    Other = 255,
}

impl InvalidReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadPow => "bad-pow",
            Self::BadBits => "bad-bits",
            Self::BadMtp => "bad-mtp",
            Self::BadMerkle => "bad-merkle",
            Self::BadTx => "bad-tx",
            Self::BadCheckpoint => "bad-checkpoint",
            Self::BadParent => "bad-parent",
            Self::Other => "other",
        }
    }

    pub fn from_code(c: u8) -> Self {
        match c {
            1 => Self::BadPow,
            2 => Self::BadBits,
            3 => Self::BadMtp,
            4 => Self::BadMerkle,
            5 => Self::BadTx,
            6 => Self::BadParent,
            7 => Self::BadCheckpoint,
            _ => Self::Other,
        }
    }
}

/// Where one transaction touching an address sits in the chain.
///
/// A position, not a claim: rows outlive reorgs, so the caller must check the
/// transaction at `index` in the body that is canonical at `height` now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrHit {
    pub height: u64,
    pub index: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddrHistory {
    /// The node was not started with addrindex, or has not indexed a block yet.
    NotIndexed,
    /// Newest first. `more` means at least one older hit exists past `hits`.
    Page { indexed_from: u64, hits: Vec<AddrHit>, more: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxLocation {
    Found { height: u64, index: u16 },
    NotIndexed { indexed_from: u64 },
    Absent,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    BodyPruned { height: u64, prune_floor: u64 },

    ForkBelowPruneFloor {
        fork_height: u64,
        prune_floor: u64,
        days_discarded: u32,
    },

    UndoExhausted {
        requested: u64,
        undo_floor: u64,
        replay_floor: u64,
    },
    NotIndexed { indexed_from: u64 },

    StateBehindHeaders {
        headers: u64,
        state: u64,
        undo_floor: u64,
    },

    SegmentDamaged {
        kind: crate::integrity::SegmentKind,
        segment: u32,
        height: u64,
        first_height: u64,
        last_height: u64,
        reason: crate::integrity::DamageKind,
    },

    UnknownWhileDegraded {
        damaged_first: u64,
        damaged_last: u64,
    },

    ReplayRangeDamaged {
        rewind_to: u64,
        fork_height: u64,
        damaged_first: u64,
        damaged_last: u64,
    },

    DatabaseAsserted {
        path: PathBuf,
        file_len: u64,
        detail: String,
    },

    IntegrityRefused {
        damaged_ranges: u32,
        first: String,
    },

    SegmentTruncated {
        file: PathBuf,
        expected_len: u64,
        actual_len: u64,
    },
    CrcMismatch {
        file: PathBuf,
        offset: u64,
        height: u64,
    },
    LinkageBroken {
        height: u64,
        expected_prev: [u8; 32],
        found_prev: [u8; 32],
    },

    TipNotInSegments {
        watermark: u64,
        window: u64,
        meta_tip: [u8; 32],
        segment_tip: [u8; 32],
    },
    EmissionMismatch {
        stored_mile: u128,
        formula_mile: u128,
    },
    StateFingerprint {
        stored: [u8; 32],
        computed: [u8; 32],
    },
    SchemaVersion {
        found: u32,
        expected: u32,
    },
    NetworkMismatch {
        found: [u8; 4],
        expected: [u8; 4],
    },

    MetaRowMalformed {
        key: &'static str,
        len: usize,
        expected: usize,
    },

    OrphanSegments {
        hdr_segments: u32,
        body_segments: u32,
        bytes: u64,
    },
    Poisoned { cause: String },
    SegmentOverflow { segment: u32, offset: u64 },
    AlreadyOpen,
    BadPlan(&'static str),
    Io(std::io::Error),
    Redb(String),
}

fn hex32(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for x in b {
        use fmt::Write;
        let _ = write!(s, "{x:02x}");
    }
    s
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyPruned { height, prune_floor } => write!(
                f,
                "body at height {height} was pruned (prune floor {prune_floor}); headers are still present"
            ),
            Self::ForkBelowPruneFloor { fork_height, prune_floor, days_discarded } => write!(
                f,
                "resync required: fork point {fork_height} is below the prune floor {prune_floor} \
                 ({days_discarded} days of bodies discarded). This node cannot reconstruct state \
                 at that height. Delete <data_dir>/segments and resync, or run against an archive peer."
            ),
            Self::UndoExhausted { requested, undo_floor, replay_floor } => write!(
                f,
                "fork point {requested} is deeper than the undo ring (floor {undo_floor}) and \
                 below the deepest state checkpoint (replay floor {replay_floor}); \
                 bodies alone cannot reconstruct a state without a base to replay from"
            ),
            Self::NotIndexed { indexed_from } => {
                write!(f, "txindex covers heights >= {indexed_from} only")
            }
            Self::StateBehindHeaders { headers, state, undo_floor } => write!(
                f,
                "headers reach {headers} but state is at {state} and the undo floor is {undo_floor}"
            ),
            Self::SegmentDamaged { kind, segment, height, first_height, last_height, reason } => write!(
                f,
                "height {height} is inside a damaged {kind:?} range: segment {segment:06x} covers \
                 {first_height}..={last_height} and is {reason:?}. This is not pruning (pruning \
                 removes a prefix and only raises prune_floor), so the node must not advertise \
                 the range as pruned or serve it as absent."
            ),
            Self::UnknownWhileDegraded { damaged_first, damaged_last } => write!(
                f,
                "by-hash lookup missed while heights {damaged_first}..={damaged_last} are damaged; \
                 a miss is not a negative answer on a degraded node"
            ),
            Self::ReplayRangeDamaged { rewind_to, fork_height, damaged_first, damaged_last } => write!(
                f,
                "forward replay {rewind_to}..={fork_height} crosses damaged bodies \
                 {damaged_first}..={damaged_last}; refetch that segment (this is not a prune-floor \
                 problem and does not need a resync)"
            ),
            Self::IntegrityRefused { damaged_ranges, first } => write!(
                f,
                "strict_integrity: refusing to start with {damaged_ranges} damaged range(s); first: {first}"
            ),
            Self::SegmentTruncated { file, expected_len, actual_len } => write!(
                f,
                "segment {} is {actual_len} bytes, watermark demands {expected_len}",
                file.display()
            ),
            Self::CrcMismatch { file, offset, height } => write!(
                f,
                "CRC32C mismatch in {} at byte offset {offset} (height {height})",
                file.display()
            ),
            Self::LinkageBroken { height, .. } => {
                write!(f, "header chain does not link at height {height}")
            }
            Self::TipNotInSegments { watermark, window, meta_tip, segment_tip } => write!(
                f,
                "chain.redb names tip {} but no header in the last {window} heights below \
                 watermark {watermark} hashes to it (the segments end at {}). redb and the \
                 segments belong to different stores; nothing has been truncated and the \
                 segments are intact. Restore the chain.redb that belongs to these segments, or \
                 resync. Do not delete chain.redb: an empty meta puts the watermark at 0 and \
                 every segment then reads as scratch.",
                hex32(meta_tip),
                hex32(segment_tip)
            ),
            Self::EmissionMismatch { stored_mile, formula_mile } => write!(
                f,
                "emission assert failed: stored {stored_mile} mile, formula {formula_mile} mile"
            ),
            Self::StateFingerprint { .. } => write!(f, "state fingerprint mismatch (bit rot or a code bug)"),
            Self::SchemaVersion { found, expected } => write!(
                f,
                "on-disk schema version {found}, this binary speaks {expected}; \
                 no silent migration and no engine fallback"
            ),
            Self::NetworkMismatch { found, expected } => {
                write!(f, "store belongs to network {found:02x?}, configured for {expected:02x?}")
            }
            Self::MetaRowMalformed { key, len, expected } => write!(
                f,
                "meta row `{key}` is {len} bytes, this schema writes {expected}; \
                 chain.redb was written by a different build or is damaged. Nothing has been \
                 modified. No silent migration and no engine fallback."
            ),
            Self::OrphanSegments { hdr_segments, body_segments, bytes } => write!(
                f,
                "chain.redb is absent, foreign, or missing its schema_version row, while \
                 {hdr_segments} header and {body_segments} body segment file(s) ({bytes} bytes) \
                 exist beside it. An empty meta puts both watermarks at 0, so every segment then \
                 reads as scratch and would be unlinked, taking the whole chain in one boot. \
                 Nothing has been deleted. Restore the chain.redb that belongs to these segments, \
                 or move <data_dir>/segments aside to resync from scratch."
            ),
            Self::DatabaseAsserted { path, file_len, detail } => write!(
                f,
                "{} is {file_len} bytes and the database engine asserted while opening it \
                 rather than returning an error ({detail}). A truncated or page-damaged \
                 chain.redb is what an interrupted write leaves behind. Nothing has been \
                 modified and no fallback engine was used. Restore the chain.redb that belongs \
                 to these segments, or move <data_dir> aside and resync.",
                path.display()
            ),
            Self::Poisoned { cause } => write!(
                f,
                "this store handle is poisoned: a destructive operation failed part way \
                 ({cause}). Its in-memory view no longer describes the store and must not be \
                 made durable. Drop the handle and reopen; the on-disk state is recoverable and \
                 open() replays the header undo blob."
            ),
            Self::SegmentOverflow { segment, offset } => {
                write!(f, "body segment {segment} offset {offset} exceeds u32")
            }
            Self::AlreadyOpen => write!(f, "the write capability has already been issued in this process"),
            Self::BadPlan(m) => write!(f, "malformed commit plan: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Redb(e) => write!(f, "redb: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

macro_rules! from_redb {
    ($($t:ty),* $(,)?) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self { Self::Redb(e.to_string()) }
        }
    )*};
}

from_redb!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SavepointError,
);
