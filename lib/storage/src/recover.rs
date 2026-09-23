use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use redb::ReadableTable;

use plaine_consensus::constants::HEADER_BYTES;
use plaine_consensus::crypto::header_hash;

use crate::anchor;
use crate::committer::{Committer, ContAcc, OPEN_GUARD};
use crate::config::{autotune_batch, StoreConfig};
use crate::error::StoreError;
use crate::layout::{self, SEG_BLOCKS};
use crate::meta::{self, Meta};
use crate::posio;
use crate::reader::{Horizons, ReaderInner, StoreReader};
use crate::segment::{self, FdCache};
use crate::tables::*;
use crate::types::{OpenReport, TipRef};
use crate::SCHEMA_VERSION;

const VERIFY_WINDOW: u64 = 256;

struct CapabilityGuard(bool);

impl Drop for CapabilityGuard {
    fn drop(&mut self) {
        if !self.0 {
            OPEN_GUARD.store(false, Ordering::SeqCst);
        }
    }
}

pub fn open(cfg: StoreConfig) -> Result<(Committer, StoreReader, OpenReport), StoreError> {
    let t0 = Instant::now();
    // One writer per process, and this guard is the entire enforcement of it: a
    // second open() is refused outright, never handed its own Committer over the
    // same segments.
    if OPEN_GUARD
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(StoreError::AlreadyOpen);
    }
    let mut guard = CapabilityGuard(false);

    let db_path = layout::db_path(&cfg.data_dir);
    let out = guard_engine_asserts(&db_path, "reading chain.redb", || open_inner(cfg, t0))??;

    guard.0 = true;
    Ok(out)
}

fn open_db_or_refuse(path: &std::path::Path, cache: usize) -> Result<redb::Database, StoreError> {
    guard_engine_asserts(path, "opening chain.redb", || {
        redb::Builder::new().set_cache_size(cache).create(path)
    })
    .and_then(|r| r.map_err(StoreError::from))
}

fn guard_engine_asserts<T>(
    path: &std::path::Path,
    stage: &'static str,
    f: impl FnOnce() -> T,
) -> Result<T, StoreError> {
    let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    type Seen = Arc<std::sync::Mutex<Option<(String, bool)>>>;
    let seen: Seen = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&seen);

    // redb aborts by panicking when it opens a torn or page-damaged file. catch
    // that panic and turn it into a named error, so a bad chain.redb refuses the
    // open instead of taking the node down. only redb's own panics are absorbed.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info.location().map(|l| l.file().to_string()).unwrap_or_default();

        // normalize the path separator so the redb-frame test works on Windows too.
        let norm = loc.replace(char::from(92), "/");
        let is_redb = norm.contains("/redb-") || norm.contains("/redb/src/");
        if let Ok(mut s) = sink.lock() {
            *s = Some((info.to_string(), is_redb));
        }
    }));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::panic::set_hook(prev);

    match res {
        Ok(v) => Ok(v),
        Err(payload) => {
            let (detail, is_redb) = seen
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .unwrap_or_else(|| ("no panic message".to_string(), false));
            if !is_redb {
                std::panic::resume_unwind(payload);
            }
            Err(StoreError::DatabaseAsserted {
                path: path.to_path_buf(),
                file_len,
                detail: format!("{stage}: {detail}"),
            })
        }
    }
}

fn open_inner(
    cfg: StoreConfig,
    t0: Instant,
) -> Result<(Committer, StoreReader, OpenReport), StoreError> {
    let root = cfg.data_dir.clone();
    std::fs::create_dir_all(layout::hdr_dir(&root))?;
    std::fs::create_dir_all(layout::body_dir(&root))?;
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(layout::lock_path(&root))?;

    let db = Arc::new(open_db_or_refuse(&layout::db_path(&root), cfg.page_cache_bytes)?);

    let mut report = OpenReport::default();
    let batch = cfg
        .ibd_batch_blocks
        .unwrap_or_else(|| autotune_batch(measure_write_bandwidth(&root)));

    if let Some(found) = meta::schema_of(&db)? {
        if found != SCHEMA_VERSION {
            return Err(StoreError::SchemaVersion {
                found,
                expected: SCHEMA_VERSION,
            });
        }
    }
    let mut m = match meta::load(&db)? {
        Some(m) => {
            debug_assert_eq!(m.schema_version, SCHEMA_VERSION, "the version gate above");
            if m.network != cfg.network.magic() {
                return Err(StoreError::NetworkMismatch {
                    found: m.network,
                    expected: cfg.network.magic(),
                });
            }
            m
        }
        None => {
            refuse_orphan_segments(&root)?;
            let m = Meta::fresh(cfg.network.magic(), 0, batch);
            let mut txn = db.begin_write()?;
            txn.set_durability(redb::Durability::Immediate);

            txn.open_table(STATE)?;
            txn.open_table(UNDO)?;
            txn.open_table(HASH_INDEX)?;
            txn.open_table(HASH_INDEX_FULL)?;
            txn.open_table(SIDE_HEADERS)?;
            txn.open_table(SIDE_BY_HEIGHT)?;
            txn.open_table(INVALID)?;
            txn.open_table(INVALID_SEQ)?;
            txn.open_table(CHAINWORK_CKPT)?;
            txn.open_table(TXINDEX)?;
            txn.open_table(ADDRINDEX)?;
            txn.open_table(HDR_UNDO)?;
            txn.open_table(STATE_CKPT)?;
            txn.open_table(BODY_ANCHOR)?;
            meta::store(&txn, &m)?;
            txn.commit()?;
            m
        }
    };
    m.ibd_batch_blocks = batch;

    let mut undo_segs: Vec<u32> = Vec::new();
    report.hdr_undo_replayed = replay_hdr_undo(&db, &root, &mut undo_segs)?;

    let w0 = m.hdr_watermark;
    let w = reconcile_headers(&root, &mut m, &mut report.hdr_scratch_discarded)?;
    if w != w0 {
        report.headers_truncated_to = Some(w);
    }

    if w < w0 {
        rollback_state_to(&db, &root, &mut m, w)?;
    }

    let bw0 = m.body_watermark;
    let (unlinked, rebuilt, live_frames) =
        reconcile_bodies(&root, &mut m, &mut report.body_scratch_discarded)?;
    report.segments_unlinked = unlinked;
    report.bidx_rebuilt_segments = rebuilt;
    if m.body_watermark != bw0 {
        report.bodies_truncated_to = Some(m.body_watermark);
    }

    if m.anchor_floor.is_none() && !cfg.anchor_mint_off {
        let bw = m.body_watermark;
        m.anchor_floor = Some(if layout::slot_of(bw) == 0 {
            bw
        } else {
            layout::seg_first_height(layout::seg_of(bw) + 1)
        });
    }

    if m.hdr_watermark > 0 && m.tip.height + 1 != m.hdr_watermark {
        return Err(StoreError::StateBehindHeaders {
            headers: m.hdr_watermark,
            state: m.tip.height,
            undo_floor: m.undo_floor,
        });
    }

    let mut integrity =
        crate::integrity::scan(&root, m.hdr_watermark, m.body_watermark, m.prune_floor);

    let mut untrusted: Vec<usize> = Vec::new();
    for (i, d) in integrity.overlong_truncated.iter().enumerate() {
        let path = match d.kind {
            crate::integrity::SegmentKind::Header => layout::hdr_seg_path(&root, d.segment),
            crate::integrity::SegmentKind::Body => {
                if !body_truncation_is_proven(&db, &root, m.network, d, &integrity.body_damage) {
                    untrusted.push(i);
                    continue;
                }
                layout::body_seg_path(&root, d.segment)
            }
        };
        if let Ok(f) = OpenOptions::new().read(true).write(true).open(&path) {
            let have = f.metadata()?.len();
            if have > d.expected_len {
                f.set_len(d.expected_len)?;
                match d.kind {
                    crate::integrity::SegmentKind::Header => {
                        report.hdr_scratch_discarded += have - d.expected_len
                    }
                    crate::integrity::SegmentKind::Body => {
                        report.body_scratch_discarded += have - d.expected_len
                    }
                }
            }
        }
    }
    for i in untrusted.into_iter().rev() {
        let d = integrity.overlong_truncated.remove(i);
        integrity.overlong_untruncated.push(d);
    }

    let mut healed: Vec<usize> = Vec::new();
    let mut alien: Vec<usize> = Vec::new();
    for (i, d) in integrity.body_damage.iter().enumerate() {
        use crate::integrity::DamageKind::*;
        if !matches!(d.reason, SidecarMissing | SidecarShort | SidecarZeroed) {
            continue;
        }
        let Ok(f) = posio::open_ro(&layout::body_seg_path(&root, d.segment)) else {
            continue;
        };
        let (entries, _) = segment::scan_body_segment(&f, SEG_BLOCKS)?;
        drop(f);
        if entries.len() as u64 != SEG_BLOCKS {
            continue;
        }
        let mut buf = Vec::with_capacity(entries.len() * 8);
        for (off, len, _) in &entries {
            buf.extend_from_slice(&crate::codec::encode_bidx(*off, *len));
        }
        if let Some(row) = anchor::get(&db, d.segment)? {
            let (_, want, _) = anchor::decode(&row);
            let Some(id) = anchor::identity(&root, d.segment) else {
                continue;
            };
            let got = anchor::geom(
                m.network,
                d.segment,
                &id,
                &buf,
                entries[0].2,
                entries[SEG_BLOCKS as usize - 1].2,
            );
            if got != want {
                alien.push(i);
                continue;
            }
        }
        let (bx, created) = posio::open_rw_create(&layout::bidx_path(&root, d.segment))?;
        if created {
            posio::sync_dir(&layout::body_dir(&root))?;
        }
        posio::pwrite_all(&bx, 0, &buf)?;
        if bx.metadata()?.len() > crate::layout::BIDX_SEG_BYTES {
            bx.set_len(crate::layout::BIDX_SEG_BYTES)?;
        }
        posio::sync_data(&bx)?;
        report.bidx_rebuilt_segments.push(d.segment);
        healed.push(i);
    }
    for i in alien {
        integrity.body_damage[i].reason = crate::integrity::DamageKind::AnchorMismatch;
    }
    for i in healed.into_iter().rev() {
        integrity.body_damage.remove(i);
    }

    let pass = anchor::verify_sealed(
        &db,
        &root,
        m.network,
        m.body_watermark,
        m.prune_floor,
        m.anchor_floor.unwrap_or(u64::MAX),
        &integrity.header_damage,
        &integrity.body_damage,
    )?;
    integrity.body_damage.extend_from_slice(&pass.damage);
    integrity.anchor_segments_checked = pass.segments_checked;
    integrity.anchors_verified = pass.verified;
    integrity.anchor_bytes_read = pass.bytes_read;
    report.anchors_verified = pass.verified;

    if let Some((old, new)) = pass.floor_raised {
        m.anchor_floor = Some(new);
        report.anchor_floor_raised = Some((old, new));
    }
    report.anchor_floor = m.anchor_floor.unwrap_or(0);
    let vouch = crate::integrity::VouchSet {
        unverifiable: pass.unverifiable.clone(),
    };
    report.unverifiable_body_ranges = pass
        .unverifiable
        .iter()
        .map(|(s, c)| {
            let first = layout::seg_first_height(*s);
            (first, first + SEG_BLOCKS - 1, *c)
        })
        .collect();

    integrity.check_micros = integrity.check_micros.max(1);

    if cfg.strict_integrity && !integrity.is_clean() {
        let first = integrity
            .header_damage
            .first()
            .or(integrity.body_damage.first())
            .map(|d| d.to_string())
            .unwrap_or_default();
        return Err(StoreError::IntegrityRefused {
            damaged_ranges: (integrity.header_damage.len() + integrity.body_damage.len()) as u32,
            first,
        });
    }

    let damage = crate::integrity::DamageSet {
        header: integrity.header_damage.clone(),
        body: integrity.body_damage.clone(),
    };

    {
        let mut txn = db.begin_write()?;
        txn.set_durability(redb::Durability::Immediate);
        report.anchors_dropped = anchor::delete_above_watermark(&txn, m.body_watermark)?;
        meta::store(&txn, &m)?;
        txn.commit()?;
    }

    let replay_floor = lowest_checkpoint(&db)?;
    let horizons = Horizons {
        hdr_watermark: m.hdr_watermark,
        body_watermark: m.body_watermark,
        prune_floor: m.prune_floor,
        undo_floor: m.undo_floor,
        replay_floor,
        issued: m.issued,
        fingerprint: m.fingerprint,
        txindex_from: m.txindex_from,
        addrindex_from: m.addrindex_from,
        hash_index_full_rows: m.hash_index_full_rows,
        hdr_damaged: !damage.header.is_empty(),
        body_damaged: !damage.body.is_empty(),
        intact_header_floor: damage.intact_header_floor(),
        anchor_floor: m.anchor_floor.unwrap_or(0),
        body_unvouched: !vouch.unverifiable.is_empty(),
    };
    let inner = Arc::new(ReaderInner {
        db: db.clone(),
        root: root.clone(),
        fds: FdCache::new(root.clone(), cfg.reader_fd_cap),
        tip: RwLock::new(m.tip),
        horizons: RwLock::new(horizons),
        damage: RwLock::new(Arc::new(damage.clone())),
        vouch: RwLock::new(Arc::new(vouch)),
        prefix_bytes: cfg.hash_prefix_bytes,
        sweep_targets: RwLock::new(Default::default()),
    });

    {
        let mut q = match inner.sweep_targets.write() {
            Ok(q) => q,
            Err(p) => p.into_inner(),
        };
        for seg in undo_segs {
            if layout::seg_first_height(seg) + layout::SEG_BLOCKS <= horizons.hdr_watermark {
                q.insert(seg);
            }
        }
    }
    let reader = StoreReader(inner.clone());

    let anchors_through = anchor::sealed_through(m.body_watermark);
    let cont = match live_frames {
        Some((seg, frames)) => ContAcc::seeded(seg, &frames),
        None => ContAcc::new(layout::seg_of(m.body_watermark)),
    };
    let mut committer = Committer::new(
        inner,
        cfg,
        m,
        replay_floor,
        damage,
        anchors_through,
        cont,
    );
    committer.meta.body_append_offset = body_append_at(&root, m.body_watermark);

    if m.index_state != 0 || m.stale_index_count >= crate::STALE_INDEX_REBUILD_THRESHOLD {
        committer.rebuild_hash_index()?;
        report.index_rebuilt = true;
    }

    report.tip = committer.tip();
    report.ibd_batch_blocks = committer.ibd_batch_blocks();
    report.integrity = integrity;
    report.open_micros = t0.elapsed().as_micros() as u64;
    Ok((committer, reader, report))
}

fn body_truncation_is_proven(
    db: &redb::Database,
    root: &Path,
    network: [u8; 4],
    d: &crate::integrity::DamagedRange,
    body_damage: &[crate::integrity::DamagedRange],
) -> bool {
    if body_damage.iter().any(|x| x.segment == d.segment) {
        return false;
    }
    let Ok(Some(row)) = anchor::get(db, d.segment) else {
        return false;
    };
    let (_, want, _) = anchor::decode(&row);
    let Some(id) = anchor::identity(root, d.segment) else {
        return false;
    };
    let Some(mat) = anchor::material(root, d.segment) else {
        return false;
    };
    anchor::geom(
        network,
        d.segment,
        &id,
        &mat.sidecar,
        mat.crc_first,
        mat.crc_last,
    ) == want
}

// segments on disk but no meta means a missing or foreign chain.redb. an empty
// meta would put both watermarks at 0 and unlink the whole chain as scratch, so
// refuse instead and let the operator restore the right redb.
fn refuse_orphan_segments(root: &Path) -> Result<(), StoreError> {
    let hdr = crate::integrity::list_hdr(root);
    let body = crate::integrity::list_bseg(root);
    if hdr.is_empty() && body.is_empty() {
        return Ok(());
    }
    Err(StoreError::OrphanSegments {
        hdr_segments: hdr.len() as u32,
        body_segments: body.len() as u32,
        bytes: hdr.values().chain(body.values()).sum(),
    })
}

fn replay_hdr_undo(
    db: &redb::Database,
    root: &Path,
    segs_out: &mut Vec<u32>,
) -> Result<Option<(u64, u32)>, StoreError> {
    let entries: Vec<(u64, Vec<u8>)> = {
        let txn = db.begin_read()?;
        let t = txn.open_table(HDR_UNDO)?;
        t.iter()?
            .map(|e| e.map(|(k, v)| (k.value(), v.value().to_vec())))
            .collect::<Result<_, _>>()?
    };
    if entries.is_empty() {
        return Ok(None);
    }
    let mut first = u64::MAX;
    for (k, v) in &entries {
        let seg = (k >> 32) as u32;
        let off = (k & 0xFFFF_FFFF) * layout::SECTOR;
        let path = layout::hdr_seg_path(root, seg);
        let (f, created) = posio::open_rw_create(&path)?;
        if created {
            posio::sync_dir(&layout::hdr_dir(root))?;
        }
        posio::pwrite_all(&f, off, v)?;
        posio::sync_data(&f)?;
        let h = layout::seg_first_height(seg) + off / HEADER_BYTES as u64;
        first = first.min(h);
        if !segs_out.contains(&seg) {
            segs_out.push(seg);
        }
    }
    let mut txn = db.begin_write()?;
    txn.set_durability(redb::Durability::Immediate);
    {
        let mut t = txn.open_table(HDR_UNDO)?;
        for (k, _) in &entries {
            t.remove(*k)?;
        }
    }
    txn.commit()?;
    Ok(Some((first, entries.len() as u32)))
}

fn reconcile_headers(root: &Path, m: &mut Meta, discarded: &mut u64) -> Result<u64, StoreError> {
    let w0 = m.hdr_watermark;
    let mut w = w0;

    if w0 > 0 && !tip_is_at(root, w0 - 1, &m.tip)? {
        if let Some(f) = verify_window(root, w0, &m.tip, true)? {
            w = f;
        }
    }
    loop {
        let keep_seg = if w == 0 { 0 } else { layout::seg_of(w - 1) };
        let lowest_scratch = keep_seg + if w == 0 { 0 } else { 1 };
        for (s, len) in crate::integrity::list_hdr(root) {
            if s < lowest_scratch {
                continue;
            }
            *discarded += len;
            std::fs::remove_file(layout::hdr_seg_path(root, s))?;
        }

        if w > 0 {
            let path = layout::hdr_seg_path(root, keep_seg);
            let want = (layout::slot_of(w - 1) + 1) * HEADER_BYTES as u64;
            match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(f) => {
                    let have = f.metadata()?.len();
                    if have > want {
                        *discarded += have - want;
                        f.set_len(want)?;
                    } else if have < want {
                        let backed = layout::seg_first_height(keep_seg) + have / HEADER_BYTES as u64;
                        if backed < w {
                            w = backed;
                            continue;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                    w = layout::seg_first_height(keep_seg);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }

        match verify_window(root, w, &m.tip, w == w0)? {
            None => break,
            Some(f) => {
                if f == 0 {
                    w = 0;
                    break;
                }
                w = f;
            }
        }
    }
    m.hdr_watermark = w;
    Ok(w)
}

// Walk the last VERIFY_WINDOW headers below the watermark along their linkage.
// Returns the first height where the chain breaks (the caller lowers the
// watermark to it), or None when the window is intact and ends at the meta tip.
fn verify_window(
    root: &Path,
    w: u64,
    tip: &TipRef,
    check_tip: bool,
) -> Result<Option<u64>, StoreError> {
    if w == 0 {
        return Ok(None);
    }
    let lo = w.saturating_sub(VERIFY_WINDOW);
    let mut prev: Option<[u8; HEADER_BYTES]> = if lo == 0 {
        None
    } else {
        read_header_at(root, lo - 1)?
    };

    let mut tip_at: Option<u64> = None;
    for h in lo..w {
        let Some(cur) = read_header_at(root, h)? else {
            return Ok(Some(h));
        };
        if let Some(p) = prev {
            if cur[12..44] != header_hash(&p) {
                return Ok(Some(h));
            }
            if check_tip && cur[12..44] == tip.hash[..] {
                tip_at = Some(h - 1);
            }
        }
        prev = Some(cur);
    }
    if !check_tip {
        return Ok(None);
    }
    let Some(last) = prev else { return Ok(None) };
    let segment_tip = header_hash(&last);
    if segment_tip == tip.hash {
        return Ok(None);
    }
    match tip_at {
        Some(t) => Ok(Some(t + 1)),
        None => Err(StoreError::TipNotInSegments {
            watermark: w,
            window: VERIFY_WINDOW.min(w),
            meta_tip: tip.hash,
            segment_tip,
        }),
    }
}

fn tip_is_at(root: &Path, h: u64, tip: &TipRef) -> Result<bool, StoreError> {
    Ok(match read_header_at(root, h)? {
        Some(hdr) => header_hash(&hdr) == tip.hash,
        None => false,
    })
}

fn read_header_at(root: &Path, h: u64) -> Result<Option<[u8; HEADER_BYTES]>, StoreError> {
    let Ok(f) = posio::open_ro(&layout::hdr_seg_path(root, layout::seg_of(h))) else {
        return Ok(None);
    };
    segment::read_header(&f, h)
}

fn rollback_state_to(
    db: &redb::Database,
    root: &Path,
    m: &mut Meta,
    w: u64,
) -> Result<(), StoreError> {
    let mut txn = db.begin_write()?;
    txn.set_durability(redb::Durability::Immediate);
    let mut h = m.tip.height;
    while h + 1 > w {
        let blob = {
            let t = txn.open_table(UNDO)?;
            let v = t.get(h)?.map(|g| g.value().to_vec());
            drop(t);
            v
        };
        let Some(blob) = blob else {
            return Err(StoreError::StateBehindHeaders {
                headers: w,
                state: m.tip.height,
                undo_floor: m.undo_floor,
            });
        };
        let (recs, issued) =
            crate::codec::decode_undo(&blob).ok_or(StoreError::BadPlan("undo blob is malformed"))?;
        {
            let mut st = txn.open_table(STATE)?;
            for r in &recs {
                if let Some(cur) = st.get(&r.addr)? {
                    let cur = crate::codec::decode_account(cur.value());
                    crate::codec::fp_xor(&mut m.fingerprint, &crate::codec::row_digest(&r.addr, &cur));
                }
                if r.existed {
                    let prev = crate::types::Account {
                        balance: r.prev_balance,
                        nonce: r.prev_nonce,
                    };
                    st.insert(&r.addr, &crate::codec::encode_account(&prev))?;
                    crate::codec::fp_xor(&mut m.fingerprint, &crate::codec::row_digest(&r.addr, &prev));
                } else {
                    st.remove(&r.addr)?;
                }
            }
        }
        {
            let mut t = txn.open_table(UNDO)?;
            t.remove(h)?;
        }

        m.issued = m
            .issued
            .checked_sub(issued)
            .ok_or(StoreError::EmissionMismatch {
                stored_mile: m.issued,
                formula_mile: issued,
            })?;
        if h == 0 {
            break;
        }
        h -= 1;
    }
    m.tip.height = w.saturating_sub(1);
    m.hdr_watermark = w;
    m.body_watermark = m.body_watermark.min(w);

    m.tip.hash = if w == 0 {
        [0u8; 32]
    } else {
        match read_header_at(root, w - 1)? {
            Some(hdr) => header_hash(&hdr),
            None => {
                let path = layout::hdr_seg_path(root, layout::seg_of(w - 1));
                let actual = std::fs::metadata(&path).map(|x| x.len()).unwrap_or(0);
                return Err(StoreError::SegmentTruncated {
                    file: path,
                    expected_len: (layout::slot_of(w - 1) + 1) * HEADER_BYTES as u64,
                    actual_len: actual,
                });
            }
        }
    };

    let base = {
        let t = txn.open_table(CHAINWORK_CKPT)?;
        let v = t
            .range(..=m.tip.height)?
            .next_back()
            .transpose()?
            .map(|(_, v)| *v.value());
        drop(t);
        v
    };
    m.tip.chainwork = if w == 0 { [0u8; 32] } else { base.unwrap_or([0u8; 32]) };

    {
        let mut t = txn.open_table(CHAINWORK_CKPT)?;
        let stale: Vec<u64> = t
            .range(w..)?
            .map(|e| e.map(|(k, _)| k.value()))
            .collect::<Result<_, _>>()?;
        for h in stale {
            t.remove(h)?;
        }
    }
    meta::store(&txn, m)?;
    txn.commit()?;
    Ok(())
}

type LiveFrames = Option<(u32, crate::segment::Frames)>;
type Reconciled = (u32, Vec<u32>, LiveFrames);

fn reconcile_bodies(root: &Path, m: &mut Meta, discarded: &mut u64) -> Result<Reconciled, StoreError> {
    let mut unlinked = 0u32;
    let bw = m.body_watermark;
    let hi = if bw == 0 { 0 } else { layout::seg_of(bw - 1) };

    let lowest_scratch = if bw == 0 { 0 } else { hi + 1 };
    let floor_seg = layout::seg_of(m.prune_floor);
    let bsegs = crate::integrity::list_bseg(root);
    let bidxs = crate::integrity::list_bidx(root);
    let mut victims: Vec<u32> = bsegs
        .keys()
        .chain(bidxs.keys())
        .copied()
        .filter(|s| *s >= lowest_scratch || *s < floor_seg)
        .collect();
    victims.sort_unstable();
    victims.dedup();
    for s in victims {
        if let Some(len) = bsegs.get(&s) {
            if s >= lowest_scratch {
                *discarded += len;
            }
        }
        let _ = std::fs::remove_file(layout::body_seg_path(root, s));
        let _ = std::fs::remove_file(layout::bidx_path(root, s));
        unlinked += 1;
    }
    if bw == 0 {
        return Ok((unlinked, Vec::new(), None));
    }

    let lo = if m.bidx_sealed_through < 0 {
        layout::seg_of(m.prune_floor)
    } else {
        (m.bidx_sealed_through as u32).max(layout::seg_of(m.prune_floor))
    };
    let mut rebuilt = Vec::new();
    let mut live: LiveFrames = None;
    for seg in lo..=hi {
        let path = layout::body_seg_path(root, seg);
        let Ok(f) = OpenOptions::new().read(true).write(true).open(&path) else {
            m.body_watermark = layout::seg_first_height(seg);
            return Ok((unlinked, rebuilt, live));
        };
        let limit = if seg == hi {
            layout::slot_of(bw - 1) + 1
        } else {
            SEG_BLOCKS
        };
        let (entries, valid_bytes) = segment::scan_body_segment(&f, limit)?;
        let have = f.metadata()?.len();
        if have > valid_bytes {
            *discarded += have - valid_bytes;
            f.set_len(valid_bytes)?;
        }
        let (bx, created) = posio::open_rw_create(&layout::bidx_path(root, seg))?;
        if created {
            posio::sync_dir(&layout::body_dir(root))?;
        }
        let mut buf = Vec::with_capacity(entries.len() * 8);
        for (off, len, _) in &entries {
            buf.extend_from_slice(&crate::codec::encode_bidx(*off, *len));
        }
        posio::pwrite_all(&bx, 0, &buf)?;
        posio::sync_data(&bx)?;
        rebuilt.push(seg);
        let backed = layout::seg_first_height(seg) + entries.len() as u64;
        if (entries.len() as u64) < limit {
            m.body_watermark = backed;
            m.bidx_sealed_through = seg as i64;
            live = Some((seg, entries));
            return Ok((unlinked, rebuilt, live));
        }
        live = Some((seg, entries));
    }
    m.bidx_sealed_through = hi as i64;
    Ok((unlinked, rebuilt, live))
}

fn body_append_at(root: &Path, body_watermark: u64) -> u32 {
    if body_watermark == 0 || layout::slot_of(body_watermark) == 0 {
        return 0;
    }
    let prev = body_watermark - 1;
    let Ok(f) = posio::open_ro(&layout::bidx_path(root, layout::seg_of(prev))) else {
        return 0;
    };
    let mut e = [0u8; 8];
    if posio::pread(&f, layout::bidx_offset(prev), &mut e).unwrap_or(0) != 8 {
        return 0;
    }
    let (off, len) = crate::codec::decode_bidx(&e);
    if len == 0 {
        return 0;
    }
    off + crate::BODY_FRAME_BYTES as u32 + len
}

fn lowest_checkpoint(db: &redb::Database) -> Result<u64, StoreError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(STATE_CKPT)?;
    let v = t.first()?.map(|(k, _)| k.value()).unwrap_or(0);
    drop(t);
    Ok(v)
}

// A quick 16 MiB fsync'd probe, just to size the IBD batch. Anything that goes
// wrong falls back to a middling 50 MB/s - we're not going to fail an open over
// a benchmark that didn't run.
fn measure_write_bandwidth(root: &Path) -> u64 {
    let path = root.join("bwprobe.tmp");
    let buf = vec![0u8; 4 * 1024 * 1024];
    let Ok((f, _)) = posio::open_rw_create(&path) else {
        return 50_000_000;
    };
    let t = Instant::now();
    for i in 0..4u64 {
        if posio::pwrite_all(&f, i * buf.len() as u64, &buf).is_err() {
            return 50_000_000;
        }
    }
    if posio::sync_data(&f).is_err() {
        return 50_000_000;
    }
    let secs = t.elapsed().as_secs_f64().max(1e-6);
    drop(f);
    let _ = std::fs::remove_file(&path);
    ((16.0 * 1024.0 * 1024.0) / secs) as u64
}

impl ReaderInner {
    pub(crate) fn header_raw(&self, h: u64) -> Result<Option<[u8; HEADER_BYTES]>, StoreError> {
        let Some(f) = self.fds.get(crate::segment::SegKind::Header, layout::seg_of(h))? else {
            return Ok(None);
        };
        let g = f.lock().expect("segment handle poisoned");
        segment::read_header(&g, h)
    }
}
