use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use redb::{ReadableTable, ReadableTableMetadata};

use crate::InvalidReason;

use plaine_consensus::constants::{HEADER_BYTES, MAX_HEADERS_PER_MSG};
use plaine_consensus::crypto::header_hash;

use crate::codec;
use crate::error::{AddrHistory, AddrHit, StoreError, TxLocation};
use crate::anchor;
use crate::integrity::{
    BodyVouch, DamageKind, DamageSet, DamagedRange, RangeAvailability, SegmentKind,
    UnverifiableCause, VouchSet,
};
use crate::layout;
use crate::segment::{self, FdCache, SegKind};
use crate::types::{Account, SideHeader, TipRef};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Horizons {
    pub hdr_watermark: u64,
    pub body_watermark: u64,
    pub prune_floor: u64,
    pub undo_floor: u64,
    pub replay_floor: u64,
    pub issued: u128,
    pub fingerprint: [u8; 32],
    pub txindex_from: Option<u64>,
    pub addrindex_from: Option<u64>,
    pub hash_index_full_rows: u64,
    pub hdr_damaged: bool,
    pub body_damaged: bool,
    pub intact_header_floor: u64,
    pub anchor_floor: u64,
    pub body_unvouched: bool,
}

pub(crate) struct ReaderInner {
    pub(crate) db: Arc<redb::Database>,
    pub(crate) root: PathBuf,
    pub(crate) fds: FdCache,
    pub(crate) tip: RwLock<TipRef>,
    pub(crate) horizons: RwLock<Horizons>,
    pub(crate) damage: RwLock<Arc<DamageSet>>,
    pub(crate) vouch: RwLock<Arc<VouchSet>>,
    pub(crate) prefix_bytes: u32,
    pub(crate) sweep_targets: RwLock<std::collections::BTreeSet<u32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    Verified,
    Unverifiable(UnverifiableCause),
}

#[derive(Debug, Clone, Copy)]
pub struct AcceptUnverified(&'static str);

impl AcceptUnverified {
    pub fn because(reason: &'static str) -> Self {
        Self(reason)
    }
    pub fn reason(&self) -> &'static str {
        self.0
    }
}

#[must_use = "a body read carries a provenance answer: call verified() or any_provenance()"]
#[derive(Debug, Clone, Copy)]
pub struct BodyRead {
    len: usize,
    proof: Proof,
}

impl BodyRead {
    pub fn proof(&self) -> Proof {
        self.proof
    }

    pub fn is_verified(&self) -> bool {
        matches!(self.proof, Proof::Verified)
    }

    pub fn verified(self) -> Result<usize, (UnverifiableCause, usize)> {
        match self.proof {
            Proof::Verified => Ok(self.len),
            Proof::Unverifiable(c) => Err((c, self.len)),
        }
    }

    pub fn any_provenance(self, _: AcceptUnverified) -> usize {
        self.len
    }
}

#[derive(Clone)]
pub struct StoreReader(pub(crate) Arc<ReaderInner>);

impl StoreReader {
    pub fn tip(&self) -> TipRef {
        *self.0.tip.read().expect("tip lock poisoned")
    }

    fn horizons(&self) -> Horizons {
        *self.0.horizons.read().expect("horizons lock poisoned")
    }

    pub fn hdr_watermark(&self) -> u64 {
        self.horizons().hdr_watermark
    }
    pub fn body_watermark(&self) -> u64 {
        self.horizons().body_watermark
    }
    pub fn prune_floor(&self) -> u64 {
        self.horizons().prune_floor
    }
    pub fn undo_floor(&self) -> u64 {
        self.horizons().undo_floor
    }

    pub fn replay_floor(&self) -> u64 {
        self.horizons().replay_floor
    }
    pub fn issued(&self) -> u128 {
        self.horizons().issued
    }
    pub fn state_fingerprint(&self) -> [u8; 32] {
        self.horizons().fingerprint
    }
    pub fn txindex_from(&self) -> Option<u64> {
        self.horizons().txindex_from
    }

    pub fn open_fd_count(&self) -> usize {
        self.0.fds.open_count()
    }

    fn damage(&self) -> Arc<DamageSet> {
        self.0.damage.read().expect("damage lock poisoned").clone()
    }

    pub fn damaged_ranges(&self) -> Vec<DamagedRange> {
        let d = self.damage();
        let mut v = Vec::with_capacity(d.header.len() + d.body.len());
        v.extend_from_slice(&d.header);
        v.extend_from_slice(&d.body);
        v
    }

    pub fn is_degraded(&self) -> bool {
        let h = self.horizons();
        h.hdr_damaged || h.body_damaged
    }

    pub fn intact_header_floor(&self) -> u64 {
        self.horizons().intact_header_floor
    }

    fn vouch(&self) -> Arc<VouchSet> {
        self.0.vouch.read().expect("vouch lock poisoned").clone()
    }

    // Anchors only cover sealed segments. A body in the live, still-growing
    // segment reads back as Unsealed rather than verified - its bytes may be
    // perfectly fine, we just have nothing sealed to check them against yet.
    fn body_proof(&self, height: u64, h: &Horizons) -> Option<UnverifiableCause> {
        if (layout::seg_of(height) as i64) > anchor::sealed_through(h.body_watermark) {
            return Some(UnverifiableCause::Unsealed {
                body_watermark: h.body_watermark,
            });
        }
        if !h.body_unvouched {
            return None;
        }
        self.vouch().cause_for(height)
    }

    pub fn body_vouch(&self) -> BodyVouch {
        let h = self.horizons();
        let v = self.vouch();
        let d = self.damage();
        let sealed = anchor::sealed_through(h.body_watermark);
        let lo = layout::seg_of(h.prune_floor);
        let mut out = BodyVouch {
            damaged: d.body.clone(),
            ..Default::default()
        };
        let mut verified: Vec<(u64, u64)> = Vec::new();
        if sealed >= lo as i64 {
            for seg in lo..=(sealed as u32) {
                let first = layout::seg_first_height(seg);
                let last = first + layout::SEG_BLOCKS - 1;
                if d.body_overlaps(first, last).is_some() {
                    continue;
                }
                match v.cause_for(first) {
                    Some(c) => out.unverifiable.push((first, last, c)),
                    None => match verified.last_mut() {
                        Some(run) if run.1 + 1 == first => run.1 = last,
                        _ => verified.push((first, last)),
                    },
                }
            }
        }

        let live_first = layout::seg_first_height((sealed + 1) as u32).max(h.prune_floor);
        if h.body_watermark > live_first {
            out.unverifiable.push((
                live_first,
                h.body_watermark - 1,
                UnverifiableCause::Unsealed {
                    body_watermark: h.body_watermark,
                },
            ));
        }
        out.verified = verified;
        out
    }

    pub fn vouches_for_all_bodies(&self) -> bool {
        let v = self.body_vouch();
        v.damaged.is_empty()
            && !v.unverifiable.iter().any(|(_, _, c)| {
                !matches!(c, UnverifiableCause::Unsealed { .. })
            })
            && !self.is_degraded()
    }

    pub fn anchor_floor(&self) -> u64 {
        self.horizons().anchor_floor
    }

    pub fn body_anchor(&self, seg: u32) -> Result<Option<[u8; 65]>, StoreError> {
        anchor::get(&self.0.db, seg)
    }

    pub fn body_anchor_count(&self) -> Result<u64, StoreError> {
        anchor::count(&self.0.db)
    }

    pub fn verify_segment_frames(&self, seg: u32) -> Result<Option<bool>, StoreError> {
        let h = self.horizons();
        if (seg as i64) > anchor::sealed_through(h.body_watermark) {
            return Ok(None);
        }
        let Some(row) = anchor::get(&self.0.db, seg)? else {
            return Ok(None);
        };
        let (_, _, want) = anchor::decode(&row);
        if want == anchor::CONT_UNAVAILABLE {
            return Ok(None);
        }
        let Some(id) = anchor::identity(&self.0.root, seg) else {
            return Ok(None);
        };
        let Some(bi) = self.0.fds.get(SegKind::Bidx, seg)? else {
            return Ok(None);
        };
        let mut sidecar = vec![0u8; layout::BIDX_SEG_BYTES as usize];
        {
            let g = bi.lock().expect("segment handle poisoned");
            if crate::posio::pread(&g, 0, &mut sidecar)? != sidecar.len() {
                return Ok(None);
            }
        }
        let Some(bf) = self.0.fds.get(SegKind::Body, seg)? else {
            return Ok(None);
        };
        let mut frames = vec![0u8; (layout::SEG_BLOCKS * 8) as usize];
        {
            let g = bf.lock().expect("segment handle poisoned");
            for slot in 0..layout::SEG_BLOCKS as usize {
                let mut e = [0u8; 8];
                e.copy_from_slice(&sidecar[slot * 8..slot * 8 + 8]);
                let (off, len) = codec::decode_bidx(&e);
                let mut fh = [0u8; 8];
                if crate::posio::pread(&g, off as u64, &mut fh)? != 8 {
                    return Ok(None);
                }
                let (flen, crc) = codec::decode_frame_header(&fh);
                if flen != len {
                    return Ok(Some(false));
                }
                frames[slot * 8..slot * 8 + 4].copy_from_slice(&len.to_le_bytes());
                frames[slot * 8 + 4..slot * 8 + 8].copy_from_slice(&crc.to_le_bytes());
            }
        }
        let net = {
            let m = crate::meta::load(&self.0.db)?;
            m.map(|m| m.network).unwrap_or([0u8; 4])
        };
        Ok(Some(anchor::cont(net, seg, &id, &frames) == want))
    }

    pub fn verify_segment_headers(&self, seg: u32) -> Result<Option<u64>, StoreError> {
        let h = self.horizons();
        let first = layout::seg_first_height(seg);
        let last = first + layout::SEG_BLOCKS - 1;

        if h.hdr_watermark < first + layout::SEG_BLOCKS {
            return Ok(None);
        }
        if h.hdr_damaged && self.damage().header_overlaps(first, last).is_some() {
            return Ok(None);
        }
        let Ok(f) = crate::posio::open_ro(&layout::hdr_seg_path(&self.0.root, seg)) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; layout::SEG_BLOCKS as usize * HEADER_BYTES];
        if crate::posio::pread(&f, 0, &mut buf)? != buf.len() {
            return Ok(None);
        }
        let rec = |b: &[u8], i: usize| -> [u8; HEADER_BYTES] {
            b[i * HEADER_BYTES..(i + 1) * HEADER_BYTES].try_into().expect("fixed stride")
        };
        let mut prev = header_hash(&rec(&buf, 0));
        let mut links = 0u64;
        for i in 1..layout::SEG_BLOCKS as usize {
            let r = rec(&buf, i);
            if r[12..44] != prev[..] {
                let mut found = [0u8; 32];
                found.copy_from_slice(&r[12..44]);
                return Err(StoreError::LinkageBroken {
                    height: first + i as u64,
                    expected_prev: prev,
                    found_prev: found,
                });
            }
            prev = header_hash(&r);
            links += 1;
        }

        if h.hdr_watermark > last + 1 {
            if let Some(next) = self.header_at(last + 1)? {
                if next[12..44] != prev[..] {
                    let mut found = [0u8; 32];
                    found.copy_from_slice(&next[12..44]);
                    return Err(StoreError::LinkageBroken {
                        height: last + 1,
                        expected_prev: prev,
                        found_prev: found,
                    });
                }
                links += 1;
            }
        }
        Ok(Some(links))
    }

    pub fn header_sweep_targets(&self) -> Vec<u32> {
        match self.0.sweep_targets.read() {
            Ok(q) => q.iter().copied().collect(),
            Err(p) => p.into_inner().iter().copied().collect(),
        }
    }

    pub fn take_header_sweep_targets(&self) -> Vec<u32> {
        let mut q = match self.0.sweep_targets.write() {
            Ok(q) => q,
            Err(p) => p.into_inner(),
        };
        std::mem::take(&mut *q).into_iter().collect()
    }

    pub fn header_availability(&self, height: u64) -> RangeAvailability {
        let h = self.horizons();
        if height >= h.hdr_watermark {
            return RangeAvailability::AboveWatermark {
                watermark: h.hdr_watermark,
            };
        }
        if h.hdr_damaged {
            if let Some(d) = self.damage().header_at(height) {
                return RangeAvailability::Damaged {
                    first: d.first_height,
                    last: d.last_height,
                    reason: d.reason,
                };
            }
        }

        RangeAvailability::Verified
    }

    pub fn body_availability(&self, height: u64) -> RangeAvailability {
        let h = self.horizons();
        if height >= h.body_watermark {
            return RangeAvailability::AboveWatermark {
                watermark: h.body_watermark,
            };
        }
        if h.body_damaged {
            if let Some(d) = self.damage().body_at(height) {
                return RangeAvailability::Damaged {
                    first: d.first_height,
                    last: d.last_height,
                    reason: d.reason,
                };
            }
        }
        if height < h.prune_floor {
            return RangeAvailability::Pruned {
                prune_floor: h.prune_floor,
            };
        }
        match self.body_proof(height, &h) {
            Some(cause) => RangeAvailability::Unverifiable { cause },
            None => RangeAvailability::Verified,
        }
    }

    fn header_hole(&self, height: u64) -> Option<StoreError> {
        if !self.horizons().hdr_damaged {
            return None;
        }
        self.damage().header_at(height).map(|d| d.into_error(height))
    }

    pub fn header_at(&self, height: u64) -> Result<Option<[u8; HEADER_BYTES]>, StoreError> {
        if height >= self.hdr_watermark() {
            return Ok(None);
        }
        if let Some(e) = self.header_hole(height) {
            return Err(e);
        }
        let Some(f) = self.0.fds.get(SegKind::Header, layout::seg_of(height))? else {
            return Err(StoreError::SegmentDamaged {
                kind: SegmentKind::Header,
                segment: layout::seg_of(height),
                height,
                first_height: layout::seg_first_height(layout::seg_of(height)),
                last_height: layout::seg_first_height(layout::seg_of(height))
                    + layout::SEG_BLOCKS
                    - 1,
                reason: DamageKind::Missing,
            });
        };
        let g = f.lock().expect("segment handle poisoned");
        match segment::read_header(&g, height)? {
            Some(h) => Ok(Some(h)),

            None => Err(StoreError::SegmentDamaged {
                kind: SegmentKind::Header,
                segment: layout::seg_of(height),
                height,
                first_height: height,
                last_height: height,
                reason: DamageKind::Short,
            }),
        }
    }

    pub fn hash_at(&self, height: u64) -> Result<Option<[u8; 32]>, StoreError> {
        Ok(self.header_at(height)?.map(|h| header_hash(&h)))
    }

    pub fn headers_range(
        &self,
        from: u64,
        count: u32,
        out: &mut Vec<u8>,
    ) -> Result<u32, StoreError> {
        out.clear();
        if count as usize > MAX_HEADERS_PER_MSG {
            return Err(StoreError::BadPlan(
                "headers_range above MAX_HEADERS_PER_MSG",
            ));
        }
        let h = self.horizons();
        let wm = h.hdr_watermark;
        if from >= wm {
            return Ok(0);
        }
        let n = (count as u64).min(wm - from);

        if n == 0 {
            return Ok(0);
        }

        if h.hdr_damaged {
            if let Some(d) = self.damage().header_overlaps(from, from + n - 1) {
                return Err(d.into_error(d.first_height.max(from)));
            }
        }
        out.reserve(n as usize * HEADER_BYTES);

        let short = |seg: u32, h: u64| StoreError::SegmentDamaged {
            kind: SegmentKind::Header,
            segment: seg,
            height: h,
            first_height: h,
            last_height: from + n - 1,
            reason: DamageKind::Short,
        };
        let mut done = 0u64;
        while done < n {
            let h = from + done;
            let seg = layout::seg_of(h);
            let in_seg = (layout::SEG_BLOCKS - layout::slot_of(h)).min(n - done);
            let Some(f) = self.0.fds.get(SegKind::Header, seg)? else {
                out.clear();
                return Err(StoreError::SegmentDamaged {
                    kind: SegmentKind::Header,
                    segment: seg,
                    height: h,
                    first_height: layout::seg_first_height(seg),
                    last_height: layout::seg_first_height(seg) + layout::SEG_BLOCKS - 1,
                    reason: DamageKind::Missing,
                });
            };
            let start = out.len();
            out.resize(start + in_seg as usize * HEADER_BYTES, 0);
            let g = f.lock().expect("segment handle poisoned");

            let got = crate::posio::pread(&g, layout::hdr_offset(h), &mut out[start..])?;
            drop(g);
            if got < in_seg as usize * HEADER_BYTES {
                out.clear();
                return Err(short(seg, h + (got / HEADER_BYTES) as u64));
            }
            done += in_seg;
        }
        Ok(done as u32)
    }

    pub fn locator(&self, out: &mut [[u8; 32]; 32]) -> Result<usize, StoreError> {
        let tip = self.tip().height;
        let hz = self.horizons();
        if hz.hdr_watermark == 0 {
            return Ok(0);
        }
        let floor = hz.intact_header_floor;
        // Standard block locator: dense near the tip, then the stride doubles on
        // the way back to genesis. log(height) hashes to pin down a fork point.
        let mut n = 0usize;
        let mut step = 1u64;
        let mut h = tip;
        loop {
            if n == out.len() || h < floor {
                break;
            }
            if let Some(hash) = self.hash_at(h)? {
                out[n] = hash;
                n += 1;
            }
            if h == 0 {
                break;
            }
            if n >= 10 {
                step = step.saturating_mul(2);
            }
            h = h.saturating_sub(step);
        }
        Ok(n)
    }

    pub fn header_by_hash(
        &self,
        hash: &[u8; 32],
    ) -> Result<Option<(u64, [u8; HEADER_BYTES])>, StoreError> {
        // short-prefix index first, full-hash overflow only on a prefix collision.
        // either way the height it hands back is confirmed against the real header
        // bytes below - the index is a hint, never the last word.
        let prefix = codec::hash_prefix_n(hash, self.0.prefix_bytes);
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::HASH_INDEX)?;
        if let Some(v) = t.get(prefix)? {
            let height = v.value();
            if let Some(hdr) = self.header_at(height)? {
                if header_hash(&hdr) == *hash {
                    return Ok(Some((height, hdr)));
                }
            }
        }

        let hz = self.horizons();
        if hz.hash_index_full_rows > 0 {
            let tf = txn.open_table(crate::tables::HASH_INDEX_FULL)?;
            if let Some(v) = tf.get(hash)? {
                let height = v.value();
                if let Some(hdr) = self.header_at(height)? {
                    if header_hash(&hdr) == *hash {
                        return Ok(Some((height, hdr)));
                    }
                }
            }
        }
        // On a degraded node a miss is not a "no". The header asked about could be
        // sitting in a damaged range, so report that rather than lie "absent".
        if hz.hdr_damaged {
            let d = self.damage();
            let first = d.header.iter().map(|r| r.first_height).min().unwrap_or(0);
            let last = d.header.iter().map(|r| r.last_height).max().unwrap_or(0);
            return Err(StoreError::UnknownWhileDegraded {
                damaged_first: first,
                damaged_last: last,
            });
        }
        Ok(None)
    }

    pub fn side_header(&self, hash: &[u8; 32]) -> Result<Option<SideHeader>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::SIDE_HEADERS)?;
        Ok(t.get(hash)?.and_then(|v| codec::decode_side(v.value())))
    }

    pub fn side_headers_from(
        &self,
        from: u64,
        max: usize,
    ) -> Result<Vec<SideHeader>, StoreError> {
        let mut out = Vec::new();
        if max == 0 {
            return Ok(out);
        }
        let txn = self.0.db.begin_read()?;
        let ix = txn.open_table(crate::tables::SIDE_BY_HEIGHT)?;
        let t = txn.open_table(crate::tables::SIDE_HEADERS)?;
        let lo = codec::side_by_height_key(from, &[0u8; 32]);
        let lo_ref: &[u8; 40] = &lo;
        for row in ix.range::<&[u8; 40]>(lo_ref..)? {
            let (k, _) = row?;
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&k.value()[8..40]);
            if let Some(v) = t.get(&hash)? {
                if let Some(s) = codec::decode_side(v.value()) {
                    out.push(s);
                }
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    pub fn is_invalid(&self, hash: &[u8; 32]) -> Result<bool, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::INVALID)?;
        Ok(t.get(hash)?.is_some())
    }

    pub fn invalid_reason(&self, hash: &[u8; 32]) -> Result<Option<InvalidReason>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::INVALID)?;
        Ok(t.get(hash)?.map(|v| InvalidReason::from_code(v.value())))
    }

    pub fn invalid_census(&self) -> Result<Vec<(InvalidReason, u64)>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::INVALID)?;
        let mut counts: std::collections::BTreeMap<u8, u64> = std::collections::BTreeMap::new();
        for row in t.iter()? {
            let (_, v) = row?;
            *counts.entry(v.value()).or_default() += 1;
        }
        Ok(counts.into_iter().map(|(c, n)| (InvalidReason::from_code(c), n)).collect())
    }

    pub fn body_at(&self, height: u64, out: &mut Vec<u8>) -> Result<Option<BodyRead>, StoreError> {
        out.clear();
        let h = self.horizons();
        if height >= h.body_watermark {
            return Ok(None);
        }
        if h.body_damaged {
            if let Some(d) = self.damage().body_at(height) {
                return Err(d.into_error(height));
            }
        }
        if height < h.prune_floor {
            return Err(StoreError::BodyPruned {
                height,
                prune_floor: h.prune_floor,
            });
        }
        let seg = layout::seg_of(height);
        let missing = |reason| StoreError::SegmentDamaged {
            kind: SegmentKind::Body,
            segment: seg,
            height,
            first_height: layout::seg_first_height(seg),
            last_height: layout::seg_first_height(seg) + layout::SEG_BLOCKS - 1,
            reason,
        };
        let Some(bi) = self.0.fds.get(SegKind::Bidx, seg)? else {
            return Err(missing(DamageKind::SidecarMissing));
        };
        let mut e = [0u8; 8];
        {
            let g = bi.lock().expect("segment handle poisoned");
            if crate::posio::pread(&g, layout::bidx_offset(height), &mut e)? != 8 {
                return Err(missing(DamageKind::SidecarShort));
            }
        }
        let (off, len) = codec::decode_bidx(&e);
        if len == 0 {
            return Err(missing(DamageKind::SidecarZeroed));
        }
        if len as usize > crate::MAX_BODY_BYTES {
            return Err(StoreError::CrcMismatch {
                file: layout::body_seg_path(&self.0.root, seg),
                offset: off as u64,
                height,
            });
        }
        let Some(bf) = self.0.fds.get(SegKind::Body, seg)? else {
            return Err(missing(DamageKind::Missing));
        };
        let mut fh = [0u8; 8];
        let g = bf.lock().expect("segment handle poisoned");
        crate::posio::pread_exact(&g, off as u64, &mut fh)?;
        let (flen, crc) = codec::decode_frame_header(&fh);
        if flen != len || flen as usize > crate::MAX_BODY_BYTES {
            return Err(StoreError::CrcMismatch {
                file: layout::body_seg_path(&self.0.root, seg),
                offset: off as u64,
                height,
            });
        }
        out.resize(flen as usize, 0);
        crate::posio::pread_exact(&g, off as u64 + 8, out)?;
        drop(g);
        // every body read re-checks its CRC; the store never trusts the disk bytes.
        if crate::crc32c::crc32c(out) != crc {
            out.clear();
            return Err(StoreError::CrcMismatch {
                file: layout::body_seg_path(&self.0.root, seg),
                offset: off as u64,
                height,
            });
        }

        Ok(Some(BodyRead {
            len: out.len(),
            proof: match self.body_proof(height, &h) {
                Some(c) => Proof::Unverifiable(c),
                None => Proof::Verified,
            },
        }))
    }

    pub fn chainwork_at(&self, height: u64) -> Result<Option<[u8; 32]>, StoreError> {
        let tip = self.tip();
        if height == tip.height && self.hdr_watermark() > 0 {
            return Ok(Some(tip.chainwork));
        }
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::CHAINWORK_CKPT)?;
        Ok(t.get(height)?.map(|v| *v.value()))
    }

    pub fn chainwork_base(&self, height: u64) -> Result<Option<(u64, [u8; 32])>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::CHAINWORK_CKPT)?;
        Ok(t.range(..=height)?
            .next_back()
            .transpose()?
            .map(|(k, v)| (k.value(), *v.value())))
    }

    pub fn account(&self, addr: &[u8; 20]) -> Result<Account, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::STATE)?;
        Ok(t.get(addr)?
            .map(|v| codec::decode_account(v.value()))
            .unwrap_or_default())
    }

    pub fn accounts(&self, addrs: &[[u8; 20]], out: &mut Vec<Account>) -> Result<(), StoreError> {
        out.clear();
        out.reserve(addrs.len());
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::STATE)?;
        for a in addrs {
            out.push(
                t.get(a)?
                    .map(|v| codec::decode_account(v.value()))
                    .unwrap_or_default(),
            );
        }
        Ok(())
    }

    pub fn undo_at(&self, height: u64) -> Result<Option<Vec<crate::types::UndoRec>>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::UNDO)?;
        Ok(t.get(height)?.and_then(|v| codec::decode_undo(v.value())).map(|(r, _)| r))
    }

    pub fn checkpoint_at_or_below(&self, height: u64) -> Result<Option<u64>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::STATE_CKPT)?;
        Ok(t.range(..=height)?
            .next_back()
            .transpose()?
            .map(|(k, _)| k.value()))
    }

    pub fn checkpoint_anchor(&self) -> Result<Option<Vec<u8>>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = match txn.open_table(crate::tables::META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(v) = t.get(crate::meta::K_ANCHOR_CP)? else { return Ok(None) };
        let raw = v.value();
        if raw.len() > crate::committer::ANCHOR_RECORD_CAP_BYTES {
            return Err(StoreError::MetaRowMalformed {
                key: crate::meta::K_ANCHOR_CP,
                len: raw.len(),
                expected: crate::committer::ANCHOR_RECORD_CAP_BYTES,
            });
        }
        Ok(Some(raw.to_vec()))
    }

    pub fn checkpoint_heights(&self) -> Result<Vec<u64>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::STATE_CKPT)?;
        let mut v = Vec::new();
        for e in t.iter()? {
            v.push(e?.0.value());
        }
        Ok(v)
    }

    pub fn addrindex_from(&self) -> Option<u64> {
        self.horizons().addrindex_from
    }

    /// Positions of the transactions that touch `addr`, newest first, at most
    /// `limit` of them, strictly older than `before` when it is given.
    pub fn addr_history(
        &self,
        addr: &[u8; 20],
        before: Option<(u64, u16)>,
        limit: usize,
    ) -> Result<AddrHistory, StoreError> {
        let Some(from) = self.addrindex_from() else {
            return Ok(AddrHistory::NotIndexed);
        };
        let txn = self.0.db.begin_read()?;
        let t = match txn.open_table(crate::tables::ADDRINDEX) {
            Ok(t) => t,
            // A store created before this table existed gets it on the first
            // indexed commit; until then there is simply nothing to read.
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(AddrHistory::Page { indexed_from: from, hits: Vec::new(), more: false })
            }
            Err(e) => return Err(e.into()),
        };
        let lo = codec::addr_key(addr, 0, 0);
        let range = match before {
            Some((h, i)) => t.range::<&[u8; 30]>(&lo..&codec::addr_key(addr, h, i))?,
            None => t.range::<&[u8; 30]>(&lo..=&codec::addr_key(addr, u64::MAX, u16::MAX))?,
        };
        let mut hits = Vec::new();
        let mut more = false;
        for row in range.rev() {
            let (k, _) = row?;
            let (_, height, index) = codec::decode_addr_key(k.value());
            if height < from {
                break;
            }
            if hits.len() == limit {
                more = true;
                break;
            }
            hits.push(AddrHit { height, index });
        }
        Ok(AddrHistory::Page { indexed_from: from, hits, more })
    }

    pub fn txindex_lookup(&self, txid: &[u8; 32]) -> Result<TxLocation, StoreError> {
        let Some(from) = self.txindex_from() else {
            return Ok(TxLocation::NotIndexed { indexed_from: u64::MAX });
        };
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::TXINDEX)?;
        let key = codec::txid_key(txid);
        match t.get(&key)? {
            Some(v) => {
                let (height, index) = codec::decode_txloc(v.value());
                if height < from {
                    Ok(TxLocation::NotIndexed { indexed_from: from })
                } else {
                    Ok(TxLocation::Found { height, index })
                }
            }
            None => Ok(TxLocation::Absent),
        }
    }

    pub fn row_counts(&self) -> Result<(u64, u64), StoreError> {
        let h = self.horizons();
        let _ = h;
        let txn = self.0.db.begin_read()?;
        let side = txn.open_table(crate::tables::SIDE_HEADERS)?.len()?;
        let inv = txn.open_table(crate::tables::INVALID)?.len()?;
        Ok((side, inv))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TableFootprint {
    pub name: &'static str,
    pub rows: u64,
    pub tree_height: u32,
    pub leaf_pages: u64,
    pub branch_pages: u64,
    pub stored_bytes: u64,
    pub metadata_bytes: u64,
    pub fragmented_bytes: u64,
}

impl TableFootprint {
    pub fn page_bytes(&self, page_size: u64) -> u64 {
        (self.leaf_pages + self.branch_pages) * page_size
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DbFootprint {
    pub allocated_pages: u64,
    pub page_size: u64,
    pub leaf_pages: u64,
    pub branch_pages: u64,
    pub stored_bytes: u64,
    pub metadata_bytes: u64,
    pub fragmented_bytes: u64,
    pub tree_height: u32,
}

impl StoreReader {
    pub fn table_footprints(&self) -> Result<Vec<TableFootprint>, StoreError> {
        let txn = self.0.db.begin_read()?;
        let mut out = Vec::new();
        macro_rules! foot {
            ($name:literal, $def:expr) => {{
                let t = txn.open_table($def)?;
                let s = t.stats()?;
                out.push(TableFootprint {
                    name: $name,
                    rows: t.len()?,
                    tree_height: s.tree_height(),
                    leaf_pages: s.leaf_pages(),
                    branch_pages: s.branch_pages(),
                    stored_bytes: s.stored_bytes(),
                    metadata_bytes: s.metadata_bytes(),
                    fragmented_bytes: s.fragmented_bytes(),
                });
            }};
        }
        foot!("state", crate::tables::STATE);
        foot!("undo", crate::tables::UNDO);
        foot!("hash_index", crate::tables::HASH_INDEX);
        foot!("hash_index_full", crate::tables::HASH_INDEX_FULL);
        foot!("side_headers", crate::tables::SIDE_HEADERS);
        foot!("side_by_height", crate::tables::SIDE_BY_HEIGHT);
        foot!("invalid", crate::tables::INVALID);
        foot!("invalid_seq", crate::tables::INVALID_SEQ);
        foot!("chainwork_ckpt", crate::tables::CHAINWORK_CKPT);
        foot!("txindex", crate::tables::TXINDEX);
        // Absent on a store created before the table existed and not indexed since.
        match txn.open_table(crate::tables::ADDRINDEX) {
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            _ => foot!("addrindex", crate::tables::ADDRINDEX),
        }
        foot!("hdr_undo", crate::tables::HDR_UNDO);
        foot!("state_ckpt", crate::tables::STATE_CKPT);

        foot!("body_anchor", crate::tables::BODY_ANCHOR);
        foot!("meta", crate::tables::META);
        Ok(out)
    }
}

const _: fn() = || {
    fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
    assert_send_sync_clone::<StoreReader>();
};

impl std::fmt::Debug for StoreReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreReader")
            .field("tip", &self.tip().height)
            .finish()
    }
}

impl StoreReader {
    pub fn verify_state_fingerprint(&self) -> Result<(), StoreError> {
        let txn = self.0.db.begin_read()?;
        let t = txn.open_table(crate::tables::STATE)?;
        let mut fp = [0u8; 32];
        for e in t.iter()? {
            let (k, v) = e?;
            let a = codec::decode_account(v.value());
            codec::fp_xor(&mut fp, &codec::row_digest(k.value(), &a));
        }
        let stored = self.state_fingerprint();
        if fp != stored {
            return Err(StoreError::StateFingerprint {
                stored,
                computed: fp,
            });
        }
        Ok(())
    }

    pub fn hash_index_full_rows(&self) -> u64 {
        self.horizons().hash_index_full_rows
    }
}
