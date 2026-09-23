#![forbid(unsafe_code)]

pub mod anchor;
mod codec;
mod committer;
mod config;
mod crc32c;
mod error;
pub mod integrity;
mod layout;
mod meta;
mod posio;
mod reader;
mod recover;
mod segment;
pub mod sweep;
mod tables;
mod types;

pub use committer::{Committer, ANCHOR_RECORD_CAP_BYTES};
pub use config::{autotune_batch, dirty_fraction, DurabilityMode, MemoryBudget, Network, StoreConfig};
pub use crc32c::crc32c;
pub use error::{AddrHistory, AddrHit, InvalidReason, StoreError, TxLocation};
pub use integrity::{
    BodyVouch, DamageKind, DamagedRange, IntegrityReport, RangeAvailability, SegmentKind,
    UnverifiableCause,
};
pub use layout::{
    bidx_offset, hdr_offset, seg_first_height, seg_of, slot_of, BIDX_SEG_BYTES, HDR_SEG_BYTES,
    SECTOR, SEG_BLOCKS, SEG_SHIFT,
};
pub use reader::{
    AcceptUnverified, BodyRead, DbFootprint, Proof, StoreReader, TableFootprint,
};

pub use posio::barriers_performed;
pub use recover::open;
pub use sweep::{
    sweep_all, sweep_segments, sweepable_range, HeaderSweepReport, HeaderSweeper, SweepTrigger,
    SweepVerdict,
};
pub use types::{
    Account, BlockToCommit, CommitReceipt, DeepReorgPlan, HeaderStatus, OpenReport, ReorgPlan,
    SideHeader, StallPoint, StateDelta, TipRef, UndoRec,
};

use plaine_consensus::constants::{HEADER_BYTES, MAX_BLOCK_BYTES, MAX_REORG_DEPTH};

pub const BODY_FRAME_BYTES: usize = 8;

pub const MAX_BODY_BYTES: usize = MAX_BLOCK_BYTES - HEADER_BYTES;

pub const UNDO_RING: u64 = 256;

pub const CHAINWORK_CKPT_SHIFT: u32 = 10;

pub const SCHEMA_VERSION: u32 = 1;

pub const SIDE_HEADERS_CAP: u64 = 16_384;

pub const INVALID_CAP: u64 = 65_536;

pub const STALE_INDEX_REBUILD_THRESHOLD: u64 = 65_536;

const _: () = assert!(
    SEG_BLOCKS * ((BODY_FRAME_BYTES + MAX_BODY_BYTES) as u64) <= u32::MAX as u64,
    "body segment must stay addressable by a u32 offset"
);

const _: () = assert!(
    UNDO_RING > MAX_REORG_DEPTH,
    "SPEC 9: the undo journal must reach deeper than MAX_REORG_DEPTH"
);

const _: () = assert!(HEADER_BYTES == 132, "SPEC 3: the header is exactly 132 bytes");
