use std::cell::Cell;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use redb::ReadableTable;

use plaine_consensus::constants::{HEADER_BYTES, MAX_REORG_DEPTH};
use plaine_consensus::crypto::header_hash;

use crate::anchor;
use crate::codec;
use crate::config::{DurabilityMode, StoreConfig};
use crate::error::{InvalidReason, StoreError};
use crate::integrity::DamageSet;
use crate::layout::{self, SECTOR, SEG_BLOCKS};
use crate::meta::Meta;
use crate::posio;
use crate::reader::{Horizons, ReaderInner};
use crate::segment::SegKind;
use crate::tables::*;
use crate::types::*;
use crate::{CHAINWORK_CKPT_SHIFT, UNDO_RING};

pub(crate) static OPEN_GUARD: AtomicBool = AtomicBool::new(false);

const MAX_BATCH_BYTES: u64 = 256 * 1024 * 1024;

pub const ANCHOR_RECORD_CAP_BYTES: usize = 2048;

pub struct Committer {
    pub(crate) inner: Arc<ReaderInner>,
    pub(crate) root: PathBuf,
    pub(crate) cfg: StoreConfig,
    pub(crate) meta: Meta,
    pub(crate) replay_floor: u64,
    seq: Cell<u64>,
    mode: DurabilityMode,
    hdr: Option<(u32, File)>,
    body: Option<(u32, File)>,
    bidx: Option<(u32, File)>,
    txn: Option<redb::WriteTransaction>,
    batch_blocks: u32,
    batch_bytes: u64,
    dirty_hdr: bool,
    dirty_body: bool,
    undo_buf: Vec<u8>,
    frame_buf: Vec<u8>,
    bytes_written: u64,
    stall: Option<Box<dyn Fn(StallPoint) + Send>>,
    abandon: bool,
    damage: DamageSet,
    anchors_through: i64,
    cont: ContAcc,
    pending_cont: Vec<(u32, Vec<u8>)>,
    poisoned: Option<String>,
}

pub(crate) struct ContAcc {
    seg: u32,
    buf: Vec<u8>,
    have: u64,
}

impl ContAcc {
    pub(crate) fn new(seg: u32) -> Self {
        Self {
            seg,
            buf: vec![0u8; (SEG_BLOCKS * 8) as usize],
            have: 0,
        }
    }

    pub(crate) fn seeded(seg: u32, frames: &crate::segment::Frames) -> Self {
        let mut a = Self::new(seg);
        for (slot, (_, len, crc)) in frames.iter().enumerate() {
            a.record(slot as u64, *len, *crc);
        }
        a
    }

    fn record(&mut self, slot: u64, len: u32, crc: u32) {
        let i = (slot * 8) as usize;
        if i + 8 > self.buf.len() {
            return;
        }
        self.buf[i..i + 4].copy_from_slice(&len.to_le_bytes());
        self.buf[i + 4..i + 8].copy_from_slice(&crc.to_le_bytes());

        self.have = slot + 1;
    }

    fn complete(&self) -> Option<&[u8]> {
        (self.have == SEG_BLOCKS).then_some(self.buf.as_slice())
    }

    fn reset(&mut self, seg: u32) {
        self.seg = seg;
        self.have = 0;
        self.buf.iter_mut().for_each(|b| *b = 0);
    }
}

impl Drop for Committer {
    fn drop(&mut self) {
        // A clean drop seals whatever is staged, so an orderly shutdown never
        // drops a committed block. abandon() is the one exit that skips the seal.
        if !self.abandon {
            let _ = self.seal(true);
        }
        OPEN_GUARD.store(false, Ordering::SeqCst);
    }
}

impl Committer {
    pub(crate) fn new(
        inner: Arc<ReaderInner>,
        cfg: StoreConfig,
        meta: Meta,
        replay_floor: u64,
        damage: DamageSet,
        anchors_through: i64,
        cont: ContAcc,
    ) -> Self {
        let root = cfg.data_dir.clone();
        Self {
            inner,
            root,
            cfg,
            meta,
            replay_floor,
            seq: Cell::new(0),
            mode: DurabilityMode::Tip,
            hdr: None,
            body: None,
            bidx: None,
            txn: None,
            batch_blocks: 0,
            batch_bytes: 0,
            dirty_hdr: false,
            dirty_body: false,
            undo_buf: Vec::new(),
            frame_buf: Vec::new(),
            bytes_written: 0,
            stall: None,
            abandon: false,
            damage,
            anchors_through,
            cont,
            pending_cont: Vec::new(),
            poisoned: None,
        }
    }

    fn guard(&self) -> Result<(), StoreError> {
        match &self.poisoned {
            None => Ok(()),
            Some(c) => Err(StoreError::Poisoned { cause: c.clone() }),
        }
    }

    fn poison(&mut self, e: StoreError) -> StoreError {
        self.poisoned = Some(e.to_string());
        e
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    pub fn set_mode(&mut self, m: DurabilityMode) -> Result<(), StoreError> {
        if m != self.mode {
            self.seal(true)?;
            self.mode = m;
        }
        Ok(())
    }

    pub fn mode(&self) -> DurabilityMode {
        self.mode
    }

    pub fn tip(&self) -> TipRef {
        self.meta.tip
    }

    pub fn prune_floor(&self) -> u64 {
        self.meta.prune_floor
    }

    pub fn undo_floor(&self) -> u64 {
        self.meta.undo_floor
    }

    pub fn replay_floor(&self) -> u64 {
        self.replay_floor
    }

    pub fn ibd_batch_blocks(&self) -> u32 {
        self.meta.ibd_batch_blocks
    }

    pub fn state_fingerprint(&self) -> [u8; 32] {
        self.meta.fingerprint
    }

    pub fn issued(&self) -> u128 {
        self.meta.issued
    }

    pub fn staging_bytes(&self) -> usize {
        self.undo_buf.capacity()
            + self.frame_buf.capacity()

            + self.cont.buf.capacity()
            + self.pending_cont.iter().map(|(_, f)| f.capacity()).sum::<usize>()
    }

    pub fn anchor_floor(&self) -> Option<u64> {
        self.meta.anchor_floor
    }

    pub fn set_stall_hook(&mut self, h: Box<dyn Fn(StallPoint) + Send>) {
        self.stall = Some(h);
    }

    pub fn abandon(mut self) {
        self.abandon = true;
        if let Some(t) = self.txn.take() {
            let _ = t.abort();
        }
    }

    fn stall(&self, p: StallPoint) {
        if let Some(h) = &self.stall {
            h(p);
        }
    }

    fn ensure_segments(&mut self, seg: u32) -> Result<(), StoreError> {
        if self.hdr.as_ref().map(|(s, _)| *s) == Some(seg) {
            return Ok(());
        }
        if self.hdr.is_some() {
            self.barrier()?;
            self.hdr = None;
            self.body = None;
            self.bidx = None;
        }
        let (hf, c1) = posio::open_rw_create(&layout::hdr_seg_path(&self.root, seg))?;
        let (bf, c2) = posio::open_rw_create(&layout::body_seg_path(&self.root, seg))?;
        let (xf, c3) = posio::open_rw_create(&layout::bidx_path(&self.root, seg))?;
        if c1 || c2 || c3 {
            posio::sync_dir(&layout::hdr_dir(&self.root))?;
            posio::sync_dir(&layout::body_dir(&self.root))?;
        }

        if self.cont.seg != seg {
            if let Some(frames) = self.cont.complete() {
                let done = (self.cont.seg, frames.to_vec());
                self.pending_cont.retain(|(s, _)| *s != done.0);
                self.pending_cont.push(done);

                // keep a handful of finished sidecars around for the anchor pass;
                // remove(0) is O(n) but n <= 4 here so it never matters.
                while self.pending_cont.len() > 4 {
                    self.pending_cont.remove(0);
                }
            }
            self.cont.reset(seg);
        }
        self.hdr = Some((seg, hf));
        self.body = Some((seg, bf));
        self.bidx = Some((seg, xf));
        self.inner.fds.evict(SegKind::Header, seg);
        self.inner.fds.evict(SegKind::Body, seg);
        self.inner.fds.evict(SegKind::Bidx, seg);

        self.stall(StallPoint::AfterSegmentSeal);
        Ok(())
    }

    fn barrier(&mut self) -> Result<(), StoreError> {
        if self.dirty_hdr {
            if let Some((_, f)) = &self.hdr {
                posio::sync_data(f)?;
            }
            self.dirty_hdr = false;
        }
        if self.dirty_body {
            if let Some((_, f)) = &self.body {
                posio::sync_data(f)?;
            }
            if let Some((s, f)) = &self.bidx {
                posio::sync_data(f)?;
                // mark the sidecar sealed only once its own fsync has landed - it
                // must never point at frames the .bseg hasn't durably backed.
                self.meta.bidx_sealed_through = *s as i64;
            }
            self.dirty_body = false;
        }
        Ok(())
    }

    pub fn extend(&mut self, blocks: &[BlockToCommit<'_>]) -> Result<CommitReceipt, StoreError> {
        let t0 = Instant::now();
        self.guard()?;
        for b in blocks {
            if b.height != self.meta.hdr_watermark {
                return Err(StoreError::BadPlan(
                    "extend must be contiguous with hdr_watermark",
                ));
            }

            check_block_inputs(b, self.cfg.addrindex)?;
            self.write_block(b).map_err(|e| self.poison(e))?;
            self.batch_blocks += 1;
            self.batch_bytes += (HEADER_BYTES + b.body.len() + crate::BODY_FRAME_BYTES) as u64;
            let limit = match self.mode {
                DurabilityMode::Tip => 1,
                DurabilityMode::Ibd => self.meta.ibd_batch_blocks,
            };
            if self.batch_blocks >= limit || self.batch_bytes >= MAX_BATCH_BYTES {
                self.seal(true).map_err(|e| self.poison(e))?;
            }
        }
        Ok(self.receipt(t0, self.txn.is_none()))
    }

    fn write_block(&mut self, b: &BlockToCommit<'_>) -> Result<(), StoreError> {
        check_block_inputs(b, self.cfg.addrindex)?;
        let seg = layout::seg_of(b.height);
        let rolled = self.hdr.as_ref().map(|(s, _)| *s) != Some(seg);
        self.ensure_segments(seg)?;
        if rolled && layout::slot_of(b.height) == 0 {
            self.meta.body_append_offset = 0;
        }

        {
            let (_, f) = self.hdr.as_ref().expect("segment open");
            posio::pwrite_all(f, layout::hdr_offset(b.height), b.header)?;
        }
        self.dirty_hdr = true;
        self.bytes_written += HEADER_BYTES as u64;
        self.stall(StallPoint::AfterHeaderWrite);

        self.write_body(b)?;
        self.stall(StallPoint::AfterBodyWrite);

        self.ensure_txn()?;
        let txn = self.txn.take().expect("txn open");
        let r = apply_block(
            &txn,
            &mut self.meta,
            &self.inner,
            b,
            self.cfg.txindex,
            self.cfg.addrindex,
            &mut self.undo_buf,
        );
        self.txn = Some(txn);
        r
    }

    fn write_body(&mut self, b: &BlockToCommit<'_>) -> Result<(), StoreError> {
        let seg = layout::seg_of(b.height);
        let off = self.meta.body_append_offset;
        let end = off as u64 + crate::BODY_FRAME_BYTES as u64 + b.body.len() as u64;
        if end > u32::MAX as u64 {
            return Err(StoreError::SegmentOverflow {
                segment: seg,
                offset: end,
            });
        }
        let crc = crate::crc32c::crc32c(b.body);
        self.frame_buf.clear();
        self.frame_buf
            .extend_from_slice(&codec::encode_frame_header(b.body.len() as u32, crc));
        self.frame_buf.extend_from_slice(b.body);
        {
            let (_, f) = self.body.as_ref().expect("segment open");
            posio::pwrite_all(f, off as u64, &self.frame_buf)?;
            let (_, x) = self.bidx.as_ref().expect("segment open");
            posio::pwrite_all(
                x,
                layout::bidx_offset(b.height),
                &codec::encode_bidx(off, b.body.len() as u32),
            )?;
        }
        if self.frame_buf.capacity() > crate::MAX_BODY_BYTES {
            self.frame_buf.shrink_to(crate::MAX_BODY_BYTES);
        }
        self.meta.body_append_offset = end as u32;
        self.cont.record(layout::slot_of(b.height), b.body.len() as u32, crc);
        self.dirty_body = true;
        self.bytes_written += (crate::BODY_FRAME_BYTES + b.body.len()) as u64;
        Ok(())
    }

    fn ensure_txn(&mut self) -> Result<(), StoreError> {
        if self.txn.is_none() {
            let mut t = self.inner.db.begin_write()?;

            // Always Immediate. We batch by keeping one txn open across many
            // blocks; redb is never asked to defer the fsync itself. DurabilityMode
            // is the whole story of how deep a batch goes.
            t.set_durability(redb::Durability::Immediate);
            self.txn = Some(t);
        }
        Ok(())
    }

    fn anchor_sync(&mut self, txn: &redb::WriteTransaction) -> Result<u32, StoreError> {
        let bw = self.meta.body_watermark;
        let top = anchor::sealed_through(bw);

        if top < self.anchors_through {
            anchor::delete_above_watermark(txn, bw)?;
            self.anchors_through = top;
            self.pending_cont.retain(|(s, _)| (*s as i64) <= top);
        }
        if self.cfg.anchor_mint_off {
            return Ok(0);
        }

        let Some(floor) = self.meta.anchor_floor else {
            return Ok(0);
        };
        let mut minted = 0u32;
        while self.anchors_through < top {
            let seg = (self.anchors_through + 1) as u32;
            let first = layout::seg_first_height(seg);

            self.anchors_through += 1;
            if first < self.meta.prune_floor || self.cfg.anchor_mint_skip.contains(&seg) {
                continue;
            }

            let grade = if first >= floor {
                anchor::GRADE_SEALED_HERE
            } else {
                anchor::GRADE_UNCHANGED_SINCE
            };
            let frames: Option<Vec<u8>> = self
                .pending_cont
                .iter()
                .find(|(s, _)| *s == seg)
                .map(|(_, f)| f.clone())
                .or_else(|| {
                    (self.cont.seg == seg)
                        .then(|| self.cont.complete().map(|f| f.to_vec()))
                        .flatten()
                });
            let Some(v) = anchor::compute(
                &self.root,
                self.meta.network,
                seg,
                grade,
                frames.as_deref(),
            ) else {
                continue;
            };
            anchor::insert(txn, seg, &v)?;
            self.pending_cont.retain(|(s, _)| *s != seg);
            minted += 1;
        }
        Ok(minted)
    }

    fn seal(&mut self, publish: bool) -> Result<(), StoreError> {
        self.guard()?;
        if self.txn.is_none() && !self.dirty_hdr && !self.dirty_body {
            return Ok(());
        }
        self.barrier()?;
        self.stall(StallPoint::BeforeRedbCommit);
        if let Some(txn) = self.txn.take() {
            let crossed = anchor::sealed_through(self.meta.body_watermark) != self.anchors_through;
            self.anchor_sync(&txn)?;
            let m = self.meta;
            crate::meta::store(&txn, &m)?;
            if crossed {
                self.stall(StallPoint::BeforeBoundaryCommit);
            }
            txn.commit()?;
            if crossed {
                self.stall(StallPoint::AfterBoundaryCommit);
            }
        }
        self.stall(StallPoint::AfterRedbCommit);
        self.batch_blocks = 0;
        self.batch_bytes = 0;
        if publish {
            self.publish();
            self.maybe_checkpoint()?;
            self.maybe_prune()?;
            self.maybe_rebuild_index()?;
        }
        Ok(())
    }

    fn publish(&self) {
        *self.inner.tip.write().expect("tip lock poisoned") = self.meta.tip;
        *self.inner.horizons.write().expect("horizons lock poisoned") = self.horizons();
    }

    fn publish_damage(&self) {
        *self.inner.damage.write().expect("damage lock poisoned") =
            Arc::new(self.damage.clone());
        self.publish();
    }

    pub(crate) fn horizons(&self) -> Horizons {
        Horizons {
            hdr_watermark: self.meta.hdr_watermark,
            body_watermark: self.meta.body_watermark,
            prune_floor: self.meta.prune_floor,
            undo_floor: self.meta.undo_floor,
            replay_floor: self.replay_floor,
            issued: self.meta.issued,
            fingerprint: self.meta.fingerprint,
            txindex_from: self.meta.txindex_from,
            addrindex_from: self.meta.addrindex_from,
            hash_index_full_rows: self.meta.hash_index_full_rows,
            hdr_damaged: !self.damage.header.is_empty(),
            body_damaged: !self.damage.body.is_empty(),
            intact_header_floor: self.damage.intact_header_floor(),
            anchor_floor: self.meta.anchor_floor.unwrap_or(0),
            body_unvouched: !self
                .inner
                .vouch
                .read()
                .expect("vouch lock poisoned")
                .unverifiable
                .is_empty(),
        }
    }

    fn retire_damage(&mut self) {
        let hw = self.meta.hdr_watermark;
        let bw = self.meta.body_watermark;
        let floor = self.meta.prune_floor;
        self.damage.header.retain(|d| d.first_height < hw);
        self.damage
            .body
            .retain(|d| d.first_height < bw && d.last_height >= floor);
    }

    pub fn damaged_ranges(&self) -> Vec<crate::integrity::DamagedRange> {
        let mut v = self.damage.header.clone();
        v.extend_from_slice(&self.damage.body);
        v
    }

    pub fn is_degraded(&self) -> bool {
        !self.damage.is_empty()
    }

    fn receipt(&self, t0: Instant, durable: bool) -> CommitReceipt {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        CommitReceipt {
            seq,
            tip: self.meta.tip,
            durable,
            bytes_written: self.bytes_written,
            micros: t0.elapsed().as_micros() as u64,
        }
    }

    pub fn flush(&mut self) -> Result<CommitReceipt, StoreError> {
        let t0 = Instant::now();
        self.seal(true)?;
        Ok(self.receipt(t0, true))
    }

    pub fn reorg(&mut self, plan: &ReorgPlan<'_>) -> Result<CommitReceipt, StoreError> {
        let t0 = Instant::now();
        self.guard()?;
        self.seal(true)?;
        let old_tip = self.meta.tip.height;

        let old_hdr_wm = self.meta.hdr_watermark;
        if plan.rollback.len() > MAX_REORG_DEPTH as usize {
            return Err(StoreError::BadPlan("rollback deeper than MAX_REORG_DEPTH"));
        }
        for w in plan.rollback.windows(2) {
            if w[0] <= w[1] {
                return Err(StoreError::BadPlan("rollback heights must be DESCENDING"));
            }
        }
        match plan.rollback.last() {
            Some(&lowest) => {
                if lowest != plan.fork_height + 1 || plan.rollback[0] != old_tip {
                    return Err(StoreError::BadPlan("rollback must cover fork_height+1..=tip"));
                }
                if lowest < self.meta.undo_floor {
                    return Err(StoreError::UndoExhausted {
                        requested: plan.fork_height,
                        undo_floor: self.meta.undo_floor,
                        replay_floor: self.replay_floor,
                    });
                }
            }
            None => {
                if plan.fork_height != old_tip {
                    return Err(StoreError::BadPlan("empty rollback with fork below tip"));
                }
            }
        }
        if plan.apply.is_empty() || plan.apply[0].height != plan.fork_height + 1 {
            return Err(StoreError::BadPlan("apply must start at fork_height+1"));
        }

        for (i, b) in plan.apply.iter().enumerate() {
            if b.height != plan.fork_height + 1 + i as u64 {
                return Err(StoreError::BadPlan("apply heights must be contiguous ascending"));
            }
            check_block_inputs(b, self.cfg.addrindex)?;
        }

        let mut old_hashes: Vec<(u64, [u8; 32])> = Vec::with_capacity(plan.rollback.len());
        for &h in plan.rollback {
            if let Some(hdr) = self.inner.header_raw(h)? {
                old_hashes.push((h, header_hash(&hdr)));
            }
        }

        // Arm the poison before we touch a single header. If we die mid-reorg the
        // in-memory view is a lie; open() must replay the undo blob, not trust it.
        self.poisoned = Some("a reorg was interrupted before its transaction committed".into());
        let out = self.reorg_destructive(plan, old_tip, &old_hashes);
        match out {
            Ok(()) => {
                self.poisoned = None;

                self.queue_header_sweep_targets(plan.fork_height, old_tip, old_hdr_wm);
            }
            Err(e) => return Err(self.poison(e)),
        }
        Ok(self.receipt(t0, true))
    }

    fn queue_header_sweep_targets(&self, fork_height: u64, old_tip: u64, old_hdr_wm: u64) {
        const TARGET_CAP: usize = 64;
        if old_tip <= fork_height {
            return;
        }
        let first = layout::seg_of(fork_height + 1);
        let last = layout::seg_of(old_tip);
        let mut q = match self.inner.sweep_targets.write() {
            Ok(q) => q,
            Err(p) => p.into_inner(),
        };
        for seg in first..=last {
            if q.len() >= TARGET_CAP {
                break;
            }
            if layout::seg_first_height(seg) + layout::SEG_BLOCKS <= old_hdr_wm {
                q.insert(seg);
            }
        }
    }

    fn reorg_destructive(
        &mut self,
        plan: &ReorgPlan<'_>,
        old_tip: u64,
        old_hashes: &[(u64, [u8; 32])],
    ) -> Result<(), StoreError> {
        let last_destructive = (plan.fork_height + plan.apply.len() as u64).min(old_tip);
        self.write_hdr_undo(plan.fork_height + 1, last_destructive)?;
        self.stall(StallPoint::ReorgAfterUndoWritten);

        self.overwrite_headers(plan.apply)?;
        self.stall(StallPoint::ReorgBeforeStateCommit);

        self.meta.body_append_offset = self.body_append_for(plan.fork_height + 1)?;
        self.commit_branch(plan.fork_height, plan.rollback, old_hashes, plan.apply)
    }

    pub fn deep_reorg(&mut self, plan: &DeepReorgPlan<'_>) -> Result<CommitReceipt, StoreError> {
        self.guard()?;
        let t0 = Instant::now();
        self.seal(true)?;
        if plan.rewind_to > plan.fork_height {
            return Err(StoreError::BadPlan("rewind_to must be <= fork_height"));
        }
        if plan.rewind_to < self.meta.prune_floor {
            let days = ((self.meta.prune_floor - plan.rewind_to) / 1_440) as u32;
            return Err(StoreError::ForkBelowPruneFloor {
                fork_height: plan.fork_height,
                prune_floor: self.meta.prune_floor,
                days_discarded: days,
            });
        }

        if let Some(d) = self
            .damage
            .body_overlaps(plan.rewind_to + 1, plan.fork_height)
        {
            return Err(StoreError::ReplayRangeDamaged {
                rewind_to: plan.rewind_to,
                fork_height: plan.fork_height,
                damaged_first: d.first_height,
                damaged_last: d.last_height,
            });
        }
        if plan.replay.len() as u64 != plan.fork_height - plan.rewind_to {
            return Err(StoreError::BadPlan(
                "replay must cover rewind_to+1..=fork_height exactly",
            ));
        }
        // Look up the savepoint id for rewind_to in its own throwaway txn, then
        // abort. redb won't let us restore a savepoint inside the transaction that
        // is still holding the table open, so the read and the restore are split.
        let sp_id = {
            let txn = self.inner.db.begin_write()?;
            let id = {
                let t = txn.open_table(STATE_CKPT)?;
                let v = t.get(plan.rewind_to)?.map(|g| g.value());
                drop(t);
                v
            };
            txn.abort()?;
            id
        };
        let Some(sp_id) = sp_id else {
            return Err(StoreError::UndoExhausted {
                requested: plan.fork_height,
                undo_floor: self.meta.undo_floor,
                replay_floor: self.replay_floor,
            });
        };

        {
            let mut txn = self.inner.db.begin_write()?;
            txn.set_durability(redb::Durability::Immediate);
            let sp = txn.get_persistent_savepoint(sp_id)?;
            txn.restore_savepoint(&sp)?;

            {
                let mut t = txn.open_table(STATE_CKPT)?;
                t.insert(plan.rewind_to, sp_id)?;
            }
            txn.commit()?;
        }
        self.meta = crate::meta::load(&self.inner.db)?
            .ok_or(StoreError::BadPlan("state checkpoint restored a store with no meta"))?;
        self.hdr = None;
        self.body = None;
        self.bidx = None;
        self.dirty_hdr = false;
        self.dirty_body = false;
        self.inner.fds.evict_all();
        self.meta.body_append_offset = self.body_append_for(plan.rewind_to + 1)?;

        self.anchors_through = anchor::sealed_through(self.meta.body_watermark);
        self.pending_cont.clear();
        self.cont = self.rebuild_cont(plan.rewind_to + 1)?;
        self.refresh_replay_floor()?;
        self.retire_damage();
        self.publish_damage();

        let saved = self.mode;
        self.mode = DurabilityMode::Ibd;
        self.extend(plan.replay)?;
        self.extend(plan.apply)?;
        self.seal(true)?;
        self.mode = saved;
        Ok(self.receipt(t0, true))
    }

    fn write_hdr_undo(&mut self, first: u64, last: u64) -> Result<(), StoreError> {
        if last < first {
            return Ok(());
        }
        let mut entries: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut h = first;
        while h <= last {
            let seg = layout::seg_of(h);
            let seg_last = (layout::seg_first_height(seg) + SEG_BLOCKS - 1).min(last);
            // Whole sectors, always. A 132-byte header can straddle a 4 KiB
            // boundary, so the undo blob has to carry every sector we're about to
            // overwrite - a partial sector would leave the neighbour unrecoverable.
            let start = layout::hdr_offset(h) / SECTOR * SECTOR;
            let end = (layout::hdr_offset(seg_last) + HEADER_BYTES as u64).div_ceil(SECTOR) * SECTOR;
            if let Ok(f) = posio::open_ro(&layout::hdr_seg_path(&self.root, seg)) {
                let mut sec = start;
                while sec < end {
                    let mut buf = vec![0u8; SECTOR as usize];
                    let n = posio::pread(&f, sec, &mut buf)?;
                    if n == 0 {
                        break;
                    }
                    buf.truncate(n);
                    entries.push(((seg as u64) << 32 | (sec / SECTOR), buf));
                    sec += SECTOR;
                }
            }
            h = seg_last + 1;
        }
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        {
            let mut t = txn.open_table(HDR_UNDO)?;
            for (k, v) in &entries {
                t.insert(*k, v.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn overwrite_headers(&mut self, apply: &[BlockToCommit<'_>]) -> Result<(), StoreError> {
        for b in apply.iter() {
            self.ensure_segments(layout::seg_of(b.height))?;
            {
                let (_, f) = self.hdr.as_ref().expect("segment open");
                posio::pwrite_all(f, layout::hdr_offset(b.height), b.header)?;
            }
            self.dirty_hdr = true;

            self.stall(StallPoint::ReorgMidHeaderOverwrite);
        }
        if self.dirty_hdr {
            if let Some((_, f)) = &self.hdr {
                posio::sync_data(f)?;
            }
            self.dirty_hdr = false;
        }
        Ok(())
    }

    fn rebuild_cont(&self, height: u64) -> Result<ContAcc, StoreError> {
        let seg = layout::seg_of(height);
        let slots = layout::slot_of(height);
        if slots == 0 {
            return Ok(ContAcc::new(seg));
        }
        let Ok(f) = posio::open_ro(&layout::body_seg_path(&self.root, seg)) else {
            return Ok(ContAcc::new(seg));
        };
        let (frames, _) = crate::segment::scan_body_segment(&f, slots)?;
        Ok(ContAcc::seeded(seg, &frames))
    }

    fn body_append_for(&self, height: u64) -> Result<u32, StoreError> {
        if height == 0 || layout::slot_of(height) == 0 {
            return Ok(0);
        }
        let prev = height - 1;
        let Ok(f) = posio::open_ro(&layout::bidx_path(&self.root, layout::seg_of(prev))) else {
            return Ok(0);
        };
        let mut e = [0u8; 8];
        if posio::pread(&f, layout::bidx_offset(prev), &mut e)? != 8 {
            return Ok(0);
        }
        let (off, len) = codec::decode_bidx(&e);
        if len == 0 {
            return Ok(0);
        }
        Ok(off + crate::BODY_FRAME_BYTES as u32 + len)
    }

    fn commit_branch(
        &mut self,
        fork_height: u64,
        rollback: &[u64],
        old_hashes: &[(u64, [u8; 32])],
        apply: &[BlockToCommit<'_>],
    ) -> Result<(), StoreError> {
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);

        {
            let mut hu = txn.open_table(HDR_UNDO)?;
            let keys: Vec<u64> = hu
                .iter()?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in keys {
                hu.remove(k)?;
            }
        }
        for &h in rollback {
            rollback_height(&txn, &mut self.meta, h)?;
        }
        {
            let mut hi = txn.open_table(HASH_INDEX)?;
            let mut hf = txn.open_table(HASH_INDEX_FULL)?;
            for (h, hash) in old_hashes {
                let p = codec::hash_prefix_n(hash, self.inner.prefix_bytes);
                if hi.get(p)?.map(|v| v.value()) == Some(*h) {
                    hi.remove(p)?;
                }
                if self.meta.hash_index_full_rows > 0 && hf.remove(hash)?.is_some() {
                    self.meta.hash_index_full_rows -= 1;
                }
            }
        }
        self.meta.stale_index_count += rollback.len() as u64;
        self.meta.tip.height = fork_height;
        self.meta.hdr_watermark = fork_height + 1;
        self.meta.body_watermark = fork_height + 1;

        let dropped = anchor::delete_above_watermark(&txn, self.meta.body_watermark)?;
        if dropped > 0 || anchor::sealed_through(self.meta.body_watermark) < self.anchors_through {
            self.anchors_through = anchor::sealed_through(self.meta.body_watermark);
            self.pending_cont.clear();
        }

        self.cont = self.rebuild_cont(fork_height + 1)?;

        for b in apply {
            check_block_inputs(b, self.cfg.addrindex)?;
            let seg = layout::seg_of(b.height);
            let rolled = self.hdr.as_ref().map(|(s, _)| *s) != Some(seg);
            self.ensure_segments(seg)?;
            if rolled && layout::slot_of(b.height) == 0 {
                self.meta.body_append_offset = 0;
            }
            self.write_body(b)?;
            apply_block(
                &txn,
                &mut self.meta,
                &self.inner,
                b,
                self.cfg.txindex,
                self.cfg.addrindex,
                &mut self.undo_buf,
            )?;
        }

        self.barrier()?;
        self.anchor_sync(&txn)?;
        {
            let m = self.meta;
            crate::meta::store(&txn, &m)?;
        }
        txn.commit()?;

        self.retire_damage();
        self.publish_damage();
        self.maybe_checkpoint()?;
        self.maybe_rebuild_index()?;
        Ok(())
    }

    fn maybe_checkpoint(&mut self) -> Result<(), StoreError> {
        let iv = self.cfg.state_ckpt_interval;
        if iv == 0 || self.meta.hdr_watermark == 0 {
            return Ok(());
        }
        let tip = self.meta.tip.height;
        {
            let txn = self.inner.db.begin_read()?;
            let t = txn.open_table(STATE_CKPT)?;
            let highest = {
                let g = t.last()?;
                g.map(|(k, _)| k.value())
            };
            drop(t);
            match highest {
                Some(h) if tip < h + iv => return Ok(()),
                Some(h) if h == tip => return Ok(()),
                _ => {}
            }
        }
        let id = {
            let mut txn = self.inner.db.begin_write()?;
            txn.set_durability(redb::Durability::Immediate);
            let id = txn.persistent_savepoint()?;
            txn.commit()?;
            id
        };
        let mut drop_ids = Vec::new();
        {
            let mut txn = self.inner.db.begin_write()?;
            txn.set_durability(redb::Durability::Immediate);
            {
                let mut t = txn.open_table(STATE_CKPT)?;
                t.insert(tip, id)?;
                let heights: Vec<u64> = t
                    .iter()?
                    .map(|e| e.map(|(k, _)| k.value()))
                    .collect::<Result<_, _>>()?;
                let keep = (self.cfg.state_ckpt_keep as usize).max(1);
                if heights.len() > keep {
                    for h in &heights[..heights.len() - keep] {
                        if let Some(v) = t.remove(h)? {
                            drop_ids.push(v.value());
                        }
                    }
                }
            }
            txn.commit()?;
        }
        if !drop_ids.is_empty() {
            let mut txn = self.inner.db.begin_write()?;
            txn.set_durability(redb::Durability::Immediate);
            for id in drop_ids {
                let _ = txn.delete_persistent_savepoint(id);
            }
            txn.commit()?;
        }
        self.refresh_replay_floor()?;
        self.publish();
        Ok(())
    }

    fn refresh_replay_floor(&mut self) -> Result<(), StoreError> {
        let txn = self.inner.db.begin_read()?;
        let t = txn.open_table(STATE_CKPT)?;
        let v = {
            let g = t.first()?;
            g.map(|(k, _)| k.value()).unwrap_or(0)
        };
        drop(t);
        self.replay_floor = v;
        Ok(())
    }

    pub fn prune_to(&mut self, floor: u64) -> Result<u32, StoreError> {
        self.guard()?;
        let target_seg = layout::seg_of(floor);
        let from_seg = layout::seg_of(self.meta.prune_floor);
        if target_seg <= from_seg {
            return Ok(0);
        }

        self.meta.prune_floor = layout::seg_first_height(target_seg);
        {
            let mut txn = self.inner.db.begin_write()?;
            txn.set_durability(redb::Durability::Immediate);
            anchor::delete_below_segment(&txn, target_seg)?;
            let m = self.meta;
            crate::meta::store(&txn, &m)?;
            txn.commit()?;
        }
        self.retire_damage();
        self.publish_damage();
        self.stall(StallPoint::PruneAfterFloorCommitted);

        let mut unlinked = 0u32;
        for s in from_seg..target_seg {
            let _ = std::fs::remove_file(layout::body_seg_path(&self.root, s));
            let _ = std::fs::remove_file(layout::bidx_path(&self.root, s));
            self.inner.fds.evict(SegKind::Body, s);
            self.inner.fds.evict(SegKind::Bidx, s);
            unlinked += 1;
            self.stall(StallPoint::PruneMidUnlink);
        }
        Ok(unlinked)
    }

    fn maybe_prune(&mut self) -> Result<(), StoreError> {
        if !self.cfg.prune {
            return Ok(());
        }
        let tip = self.meta.tip.height;
        let retain = self.cfg.body_retain_blocks;
        if tip < retain + SEG_BLOCKS {
            return Ok(());
        }

        let target = tip - retain;
        if target > self.meta.prune_floor + SEG_BLOCKS {
            self.prune_to(target)?;
        }
        Ok(())
    }

    pub fn repair_headers(
        &mut self,
        first: u64,
        headers: &[[u8; HEADER_BYTES]],
    ) -> Result<u32, StoreError> {
        self.guard()?;
        self.seal(true)?;
        let n = headers.len() as u64;
        if n == 0 {
            return Err(StoreError::BadPlan("repair_headers with no headers"));
        }
        let last = first + n - 1;
        let Some(idx) = self
            .damage
            .header
            .iter()
            .position(|d| d.first_height == first && d.last_height == last)
        else {
            return Err(StoreError::BadPlan(
                "repair_headers must cover exactly one damaged range, whole",
            ));
        };
        if layout::seg_of(first) != layout::seg_of(last) {
            return Err(StoreError::BadPlan("a damaged range never spans segments"));
        }

        let succ = last + 1;
        if succ >= self.meta.hdr_watermark || self.damage.header_at(succ).is_some() {
            return Err(StoreError::BadPlan(
                "repair needs an intact successor: the upper anchor is what pins the run",
            ));
        }
        let Some(succ_hdr) = self.inner.header_raw(succ)? else {
            return Err(StoreError::BadPlan("the successor header is not readable"));
        };

        for i in 1..headers.len() {
            if headers[i][12..44] != header_hash(&headers[i - 1])[..] {
                return Err(StoreError::LinkageBroken {
                    height: first + i as u64,
                    expected_prev: header_hash(&headers[i - 1]),
                    found_prev: {
                        let mut p = [0u8; 32];
                        p.copy_from_slice(&headers[i][12..44]);
                        p
                    },
                });
            }
        }

        if succ_hdr[12..44] != header_hash(&headers[headers.len() - 1])[..] {
            let mut p = [0u8; 32];
            p.copy_from_slice(&succ_hdr[12..44]);
            return Err(StoreError::LinkageBroken {
                height: succ,
                expected_prev: header_hash(&headers[headers.len() - 1]),
                found_prev: p,
            });
        }

        if first > 0 && self.damage.header_at(first - 1).is_none() {
            if let Some(prev) = self.inner.header_raw(first - 1)? {
                if headers[0][12..44] != header_hash(&prev)[..] {
                    let mut p = [0u8; 32];
                    p.copy_from_slice(&headers[0][12..44]);
                    return Err(StoreError::LinkageBroken {
                        height: first,
                        expected_prev: header_hash(&prev),
                        found_prev: p,
                    });
                }
            }
        }

        let seg = layout::seg_of(first);
        let (f, created) = posio::open_rw_create(&layout::hdr_seg_path(&self.root, seg))?;
        if created {
            posio::sync_dir(&layout::hdr_dir(&self.root))?;
        }
        for (i, h) in headers.iter().enumerate() {
            posio::pwrite_all(&f, layout::hdr_offset(first + i as u64), h)?;
        }
        posio::sync_data(&f)?;
        drop(f);
        self.inner.fds.evict(SegKind::Header, seg);
        for (i, want) in headers.iter().enumerate() {
            let got = self
                .inner
                .header_raw(first + i as u64)?
                .ok_or(StoreError::BadPlan("repaired header did not read back"))?;
            if got != *want {
                return Err(StoreError::BadPlan("repaired header read back different"));
            }
        }

        {
            let mut txn = self.inner.db.begin_write()?;
            txn.set_durability(redb::Durability::Immediate);
            {
                let mut hi = txn.open_table(HASH_INDEX)?;
                let mut hf = txn.open_table(HASH_INDEX_FULL)?;
                for (i, h) in headers.iter().enumerate() {
                    let height = first + i as u64;
                    let hash = header_hash(h);
                    let p = codec::hash_prefix_n(&hash, self.inner.prefix_bytes);
                    let existing = {
                        let g = hi.get(p)?;
                        g.map(|x| x.value())
                    };
                    match existing {
                        None => {
                            hi.insert(p, height)?;
                        }
                        Some(h0) if h0 == height => {}
                        Some(_) => {
                            if hf.insert(&hash, height)?.is_none() {
                                self.meta.hash_index_full_rows += 1;
                            }
                        }
                    }
                }
            }
            let m = self.meta;
            crate::meta::store(&txn, &m)?;
            txn.commit()?;
        }

        self.damage.header.remove(idx);
        self.publish_damage();
        Ok(n as u32)
    }

    fn maybe_rebuild_index(&mut self) -> Result<(), StoreError> {
        if self.meta.index_state == 0
            && self.meta.stale_index_count < crate::STALE_INDEX_REBUILD_THRESHOLD
        {
            return Ok(());
        }
        self.rebuild_hash_index()
    }

    pub fn rebuild_hash_index(&mut self) -> Result<(), StoreError> {
        self.guard()?;
        self.seal(true)?;
        let wm = self.meta.hdr_watermark;
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        {
            let mut hi = txn.open_table(HASH_INDEX)?;
            let mut hf = txn.open_table(HASH_INDEX_FULL)?;
            let keys: Vec<u64> = hi
                .iter()?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in keys {
                hi.remove(k)?;
            }
            let fkeys: Vec<[u8; 32]> = hf
                .iter()?
                .map(|e| e.map(|(k, _)| *k.value()))
                .collect::<Result<_, _>>()?;
            for k in &fkeys {
                hf.remove(k)?;
            }
            self.meta.hash_index_full_rows = 0;
            for h in 0..wm {
                let Some(hdr) = self.inner.header_raw(h)? else {
                    continue;
                };
                let hash = header_hash(&hdr);
                let p = codec::hash_prefix_n(&hash, self.inner.prefix_bytes);
                if hi.get(p)?.is_some() {
                    hf.insert(&hash, h)?;
                    self.meta.hash_index_full_rows += 1;
                } else {
                    hi.insert(p, h)?;
                }
            }
        }
        self.meta.index_state = 0;
        self.meta.stale_index_count = 0;
        {
            let m = self.meta;
            crate::meta::store(&txn, &m)?;
        }
        txn.commit()?;
        self.publish();
        Ok(())
    }

    pub fn db_footprint(&mut self) -> Result<crate::reader::DbFootprint, StoreError> {
        self.seal(false)?;
        let txn = self.inner.db.begin_write()?;
        let s = txn.stats()?;
        let out = crate::reader::DbFootprint {
            allocated_pages: s.allocated_pages(),
            page_size: s.page_size() as u64,
            leaf_pages: s.leaf_pages(),
            branch_pages: s.branch_pages(),
            stored_bytes: s.stored_bytes(),
            metadata_bytes: s.metadata_bytes(),
            fragmented_bytes: s.fragmented_bytes(),
            tree_height: s.tree_height(),
        };
        txn.abort()?;
        Ok(out)
    }

    pub fn verify_emission_against_formula(&self) -> Result<(), StoreError> {
        if self.meta.hdr_watermark == 0 {
            return Ok(());
        }
        let formula = plaine_consensus::emission::issued_through(self.meta.tip.height);
        if formula != self.meta.issued {
            return Err(StoreError::EmissionMismatch {
                stored_mile: self.meta.issued,
                formula_mile: formula,
            });
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn mark_index_stale(&mut self) -> Result<(), StoreError> {
        self.guard()?;
        self.seal(false)?;
        self.meta.index_state = 1;
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        let m = self.meta;
        crate::meta::store(&txn, &m)?;
        txn.commit()?;
        Ok(())
    }

    pub fn put_side_header(
        &mut self,
        hash: &[u8; 32],
        hdr: &[u8; HEADER_BYTES],
        height: u64,
        st: HeaderStatus,
    ) -> Result<(), StoreError> {
        self.put_side_headers(&[(*hash, *hdr, height, st)])
    }

    pub fn put_side_headers(
        &mut self,
        rows: &[([u8; 32], [u8; HEADER_BYTES], u64, HeaderStatus)],
    ) -> Result<(), StoreError> {
        self.guard()?;
        self.seal(false)?;
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        {
            let mut t = txn.open_table(SIDE_HEADERS)?;
            let mut ix = txn.open_table(SIDE_BY_HEIGHT)?;
            for (hash, hdr, height, st) in rows {
                if t.insert(hash, &codec::encode_side(hdr, *height, *st))?.is_none() {
                    self.meta.side_rows += 1;
                }
                ix.insert(&codec::side_by_height_key(*height, hash), ())?;
            }

            while self.meta.side_rows > self.cfg.side_headers_cap {
                let victim = {
                    let g = ix.iter()?.next().transpose()?;
                    g.map(|(k, _)| *k.value())
                };
                let Some(k) = victim else { break };
                ix.remove(&k)?;
                let mut vh = [0u8; 32];
                vh.copy_from_slice(&k[8..40]);
                if t.remove(&vh)?.is_some() {
                    self.meta.side_rows -= 1;
                }
            }
        }
        let m = self.meta;
        crate::meta::store(&txn, &m)?;
        txn.commit()?;
        Ok(())
    }

    pub fn put_checkpoint_anchor(&mut self, raw: &[u8]) -> Result<(), StoreError> {
        self.guard()?;
        if raw.is_empty() || raw.len() > ANCHOR_RECORD_CAP_BYTES {
            return Err(StoreError::BadPlan("anchor record length out of range"));
        }
        self.seal(false)?;
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        {
            let mut t = txn.open_table(crate::tables::META)?;
            t.insert(crate::meta::K_ANCHOR_CP, raw)?;
        }
        let m = self.meta;
        crate::meta::store(&txn, &m)?;
        txn.commit()?;
        Ok(())
    }

    pub fn mark_invalid(&mut self, hash: &[u8; 32], r: InvalidReason) -> Result<(), StoreError> {
        self.mark_invalid_batch(&[(*hash, r)])
    }

    pub fn mark_invalid_batch(&mut self, rows: &[([u8; 32], InvalidReason)]) -> Result<(), StoreError> {
        self.guard()?;
        self.seal(false)?;
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        {
            let mut t = txn.open_table(INVALID)?;
            let mut sq = txn.open_table(INVALID_SEQ)?;
            for (hash, r) in rows {
                if t.insert(hash, *r as u8)?.is_none() {
                    sq.insert(self.meta.invalid_next_seq, hash)?;
                    self.meta.invalid_next_seq += 1;
                    self.meta.invalid_rows += 1;
                }
            }
            while self.meta.invalid_rows > self.cfg.invalid_cap {
                let victim = {
                    let g = sq.iter()?.next().transpose()?;
                    g.map(|(k, v)| (k.value(), *v.value()))
                };
                let Some((k, h)) = victim else { break };
                sq.remove(k)?;
                if t.remove(&h)?.is_some() {
                    self.meta.invalid_rows -= 1;
                }
            }
        }
        let m = self.meta;
        crate::meta::store(&txn, &m)?;
        txn.commit()?;
        Ok(())
    }

    pub fn clear_invalid_all(&mut self) -> Result<u32, StoreError> {
        self.guard()?;
        self.seal(false)?;
        let mut n = 0u32;
        let mut txn = self.inner.db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        {
            let mut t = txn.open_table(INVALID)?;
            let mut sq = txn.open_table(INVALID_SEQ)?;
            let keys: Vec<[u8; 32]> = t
                .iter()?
                .map(|e| e.map(|(k, _)| *k.value()))
                .collect::<Result<_, _>>()?;
            for k in &keys {
                t.remove(k)?;
                n += 1;
            }
            let sk: Vec<u64> = sq
                .iter()?
                .map(|e| e.map(|(k, _)| k.value()))
                .collect::<Result<_, _>>()?;
            for k in sk {
                sq.remove(k)?;
            }
        }
        self.meta.invalid_rows = 0;
        let m = self.meta;
        crate::meta::store(&txn, &m)?;
        txn.commit()?;
        Ok(n)
    }
}

fn check_block_inputs(b: &BlockToCommit<'_>, addrindex: bool) -> Result<(), StoreError> {
    debug_assert_eq!(
        header_hash(b.header),
        b.hash,
        "the validator's hash and the header bytes disagree"
    );
    if b.deltas.len() != b.undo.len() {
        return Err(StoreError::BadPlan("deltas and undo must be paired"));
    }
    for (d, u) in b.deltas.iter().zip(b.undo.iter()) {
        if d.addr != u.addr {
            return Err(StoreError::BadPlan("deltas and undo must be address-aligned"));
        }
    }
    if b.body.len() > crate::MAX_BODY_BYTES {
        return Err(StoreError::BadPlan("body exceeds MAX_BLOCK_BYTES - HEADER_BYTES"));
    }

    if b.body.is_empty() {
        return Err(StoreError::BadPlan(
            "body must be non-empty: len == 0 is the sidecar's absent sentinel",
        ));
    }
    // With the address index on, the body is parsed during apply. Refuse it here,
    // before anything is written, rather than halfway through a block's rows.
    if addrindex && plaine_consensus::codec::BlockBody::parse(b.body).is_err() {
        return Err(StoreError::BadPlan(
            "body does not parse, so its addresses cannot be indexed",
        ));
    }
    Ok(())
}

fn apply_block(
    txn: &redb::WriteTransaction,
    meta: &mut Meta,
    inner: &ReaderInner,
    b: &BlockToCommit<'_>,
    txindex: bool,
    addrindex: bool,
    undo_buf: &mut Vec<u8>,
) -> Result<(), StoreError> {
    let prefix_bytes = inner.prefix_bytes;
    {
        let mut st = txn.open_table(STATE)?;
        for (d, u) in b.deltas.iter().zip(b.undo.iter()) {
            if d.addr != u.addr {
                return Err(StoreError::BadPlan("deltas and undo must be address-aligned"));
            }
            // fold the old row out here, the new row in below: the running XOR
            // stays equal to a from-scratch digest of the live state table.
            if u.existed {
                let prev = Account {
                    balance: u.prev_balance,
                    nonce: u.prev_nonce,
                };
                codec::fp_xor(&mut meta.fingerprint, &codec::row_digest(&u.addr, &prev));
            }
            let next = Account {
                balance: d.balance,
                nonce: d.nonce,
            };
            if next.is_absent() {
                st.remove(&d.addr)?;
            } else {
                st.insert(&d.addr, &codec::encode_account(&next))?;
                codec::fp_xor(&mut meta.fingerprint, &codec::row_digest(&d.addr, &next));
            }
        }
    }
    {
        let mut un = txn.open_table(UNDO)?;
        codec::encode_undo(b.undo, b.issued_delta, undo_buf);
        un.insert(b.height, undo_buf.as_slice())?;
        // The undo journal is a ring of the last UNDO_RING heights. Anything
        // deeper is reconstructed by replaying forward from a state checkpoint,
        // so drop the row that just fell off the bottom.
        if b.height >= UNDO_RING {
            un.remove(b.height - UNDO_RING)?;
        }
        if undo_buf.capacity() > codec::MAX_UNDO_BLOB_BYTES {
            undo_buf.shrink_to(codec::MAX_UNDO_BLOB_BYTES);
        }
    }
    {
        let mut hi = txn.open_table(HASH_INDEX)?;
        let mut hf = txn.open_table(HASH_INDEX_FULL)?;
        let p = codec::hash_prefix_n(&b.hash, prefix_bytes);
        // HACK: copy the value out and drop the read guard in the same statement -
        // the guard borrows `hi` immutably and we need it &mut a few lines down.
        let existing = { let g = hi.get(p)?; let v = g.map(|x| x.value()); v };
        match existing {
            None => {
                hi.insert(p, b.height)?;
            }
            Some(h0) if h0 == b.height => {}
            Some(h0) => {
                // prefix clash. keep the incumbent in the short index if it is still
                // a real header; the newcomer then goes to the full-hash overflow.
                let incumbent_still_valid = match inner.header_raw(h0)? {
                    Some(hdr) => codec::hash_prefix_n(&header_hash(&hdr), prefix_bytes) == p,
                    None => false,
                };
                if incumbent_still_valid {
                    if hf.insert(&b.hash, b.height)?.is_none() {
                        meta.hash_index_full_rows += 1;
                    }
                } else {
                    hi.insert(p, b.height)?;
                }
            }
        }
    }
    // one chainwork sample every 2^CHAINWORK_CKPT_SHIFT heights; a caller walks
    // forward from the nearest base to fill the gap.
    if b.height & ((1u64 << CHAINWORK_CKPT_SHIFT) - 1) == 0 {
        let mut cw = txn.open_table(CHAINWORK_CKPT)?;
        cw.insert(b.height, &b.chainwork)?;
    }
    if txindex {
        if let Some(ids) = b.txids {
            let mut tx = txn.open_table(TXINDEX)?;
            for (i, id) in ids.iter().enumerate() {
                tx.insert(&codec::txid_key(id), &codec::encode_txloc(b.height, i as u16))?;
            }
            if meta.txindex_from.is_none() {
                meta.txindex_from = Some(b.height);
            }
        }
    }
    if addrindex {
        index_addresses(txn, b)?;
        if meta.addrindex_from.is_none() {
            meta.addrindex_from = Some(b.height);
        }
    }
    meta.issued = meta.issued.saturating_add(b.issued_delta);
    meta.tip = TipRef {
        hash: b.hash,
        height: b.height,
        chainwork: b.chainwork,
    };
    meta.hdr_watermark = b.height + 1;
    meta.body_watermark = b.height + 1;
    meta.undo_floor = (b.height + 1).saturating_sub(UNDO_RING);
    Ok(())
}

// One row per (party, block position). A transfer to oneself is one row, since
// both parties give the same key. Rows are not removed on rollback: a reorg
// replaces the body at a height, and readers check every hit against the body
// that is canonical now, exactly as txindex lookups do.
fn index_addresses(txn: &redb::WriteTransaction, b: &BlockToCommit<'_>) -> Result<(), StoreError> {
    use plaine_consensus::codec::{BlockBody, Tx};
    use plaine_consensus::crypto::address_payload;

    let body = BlockBody::parse(b.body)
        .map_err(|_| StoreError::BadPlan("body does not parse, so its addresses cannot be indexed"))?;
    let mut t = txn.open_table(ADDRINDEX)?;
    for i in 0..body.len() {
        let index = u16::try_from(i)
            .map_err(|_| StoreError::BadPlan("more than 65535 transactions in one block"))?;
        let parties = match body.decode_tx(i) {
            Some(Ok(Tx::Coinbase(cb))) => [Some(cb.to), None],
            Some(Ok(Tx::Transfer(tx))) => [Some(address_payload(&tx.from_pub)), Some(tx.to)],
            Some(Ok(Tx::Announcement(a))) => [Some(address_payload(&a.from_pub)), None],
            // A type this build does not know is length-delimited in the body and
            // skipped, as everywhere else in the node.
            _ => [None, None],
        };
        for addr in parties.into_iter().flatten() {
            t.insert(&codec::addr_key(&addr, b.height, index), ())?;
        }
    }
    Ok(())
}

fn rollback_height(
    txn: &redb::WriteTransaction,
    meta: &mut Meta,
    h: u64,
) -> Result<(), StoreError> {
    let blob = {
        let un = txn.open_table(UNDO)?;
        let v = un.get(h)?.map(|g| g.value().to_vec());
        drop(un);
        v
    };
    let Some(blob) = blob else {
        return Err(StoreError::UndoExhausted {
            requested: h,
            undo_floor: meta.undo_floor,
            replay_floor: 0,
        });
    };
    let (recs, issued_delta) =
        codec::decode_undo(&blob).ok_or(StoreError::BadPlan("undo blob is malformed"))?;

    meta.issued = meta
        .issued
        .checked_sub(issued_delta)
        .ok_or(StoreError::EmissionMismatch {
            stored_mile: meta.issued,
            formula_mile: issued_delta,
        })?;
    {
        let mut st = txn.open_table(STATE)?;
        for r in &recs {
            if let Some(cur) = st.get(&r.addr)? {
                let cur = codec::decode_account(cur.value());
                codec::fp_xor(&mut meta.fingerprint, &codec::row_digest(&r.addr, &cur));
            }
            if r.existed {
                let prev = Account {
                    balance: r.prev_balance,
                    nonce: r.prev_nonce,
                };
                st.insert(&r.addr, &codec::encode_account(&prev))?;
                codec::fp_xor(&mut meta.fingerprint, &codec::row_digest(&r.addr, &prev));
            } else {
                st.remove(&r.addr)?;
            }
        }
    }
    {
        let mut un = txn.open_table(UNDO)?;
        un.remove(h)?;
    }
    {
        let mut cw = txn.open_table(CHAINWORK_CKPT)?;
        if h & ((1u64 << CHAINWORK_CKPT_SHIFT) - 1) == 0 {
            cw.remove(h)?;
        }
    }
    Ok(())
}
