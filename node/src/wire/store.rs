use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use plaine_chain::traits::{Sink, SinkError, Store};
use plaine_chain::types::{
    Account, Address, CommitBlock, DeepReorgCommit, Hash32, HeaderRec, Receipt, ReorgCommit,
    SideHeaderRec, SignedCheckpoint, TipRef, UndoRec,
};
use plaine_consensus::constants::HEADER_BYTES;
use plaine_storage::{
    AcceptUnverified, BlockToCommit, Committer, DeepReorgPlan, ReorgPlan, StoreReader,
};

pub const RING_MAX_BLOCKS: usize = 256;

const _: () = assert!(
    RING_MAX_BLOCKS as u64 > plaine_consensus::constants::COINBASE_MATURITY,
    "the unsealed ring must outreach the depth build_ledger walks, or the immature-coinbase \
     ledger comes back short and a coinbase becomes spendable early"
);
const _: () = assert!(
    RING_MAX_BLOCKS as u64 > plaine_consensus::constants::MAX_REORG_DEPTH,
    "the unsealed ring must outreach the depth rewind_to_fork probes undo rows over"
);

pub const RING_MAX_BYTES: usize = 24 * 1024 * 1024;

struct Entry {
    height: u64,
    hash: Hash32,
    header: [u8; HEADER_BYTES],
    body: Vec<u8>,
    deltas: Vec<(Address, Account)>,
    undo: Vec<UndoRec>,
    issued_delta: u128,
}

#[derive(Default)]
pub struct UnsealedInner {
    entries: Vec<Entry>,
    by_hash: HashMap<Hash32, usize>,
    bytes: usize,
}

pub type Unsealed = Arc<RwLock<UnsealedInner>>;

pub fn new_ring() -> Unsealed {
    Arc::new(RwLock::new(UnsealedInner::default()))
}

impl UnsealedInner {
    fn push(&mut self, e: Entry) {
        self.bytes += e.body.len();
        self.by_hash.insert(e.hash, self.entries.len());
        self.entries.push(e);
    }

    fn prune(&mut self, watermark: u64) {
        if self.entries.is_empty() {
            return;
        }
        let keep: Vec<Entry> = self
            .entries
            .drain(..)
            .filter(|e| e.height >= watermark)
            .collect();
        self.reindex(keep);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.by_hash.clear();
        self.bytes = 0;
    }

    fn reindex(&mut self, entries: Vec<Entry>) {
        self.by_hash.clear();
        self.bytes = 0;
        self.entries = entries;
        for (i, e) in self.entries.iter().enumerate() {
            self.by_hash.insert(e.hash, i);
            self.bytes += e.body.len();
        }
    }

    // newest-first: after a reorg inside the window, the latest entry at a height
    // wins over an older one still sitting in the ring.
    fn at(&self, height: u64) -> Option<&Entry> {
        self.entries.iter().rev().find(|e| e.height == height)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

// The ring holds the unsealed tail of the current chain. Every read checks it
// before falling back to the sealed reader, so a block just committed is visible
// before it ever reaches disk.
pub struct NodeStore {
    reader: StoreReader,
    ring: Unsealed,
}

impl NodeStore {
    pub fn new(reader: StoreReader, ring: Unsealed) -> NodeStore {
        NodeStore { reader, ring }
    }

    pub fn reader(&self) -> &StoreReader {
        &self.reader
    }

    pub fn ring(&self) -> &Unsealed {
        &self.ring
    }

    pub fn invalid_reason(&self, h: &Hash32) -> Option<plaine_storage::InvalidReason> {
        self.reader.invalid_reason(h).ok().flatten()
    }

    pub fn body_at_verified(&self, height: u64) -> Option<Vec<u8>> {
        if let Some(b) = self.ring.read().ok()?.at(height).map(|e| e.body.clone()) {
            return Some(b);
        }
        let mut buf = Vec::new();
        match self.reader.body_at(height, &mut buf) {
            Ok(Some(r)) => {
                let n = r.any_provenance(AcceptUnverified::because(
                    "our own chain: the frame CRC passed on this read, and this body already \
                     satisfied its header's tx_root when it connected",
                ));
                buf.truncate(n);
                Some(buf)
            }
            _ => None,
        }
    }

    pub fn tx_in_ring(&self, txid: &Hash32) -> Option<(u64, u16, Vec<u8>)> {
        let g = self.ring.read().ok()?;
        for e in g.entries.iter().rev() {
            if g.at(e.height).map(|c| c.hash) != Some(e.hash) {
                continue;
            }
            let Ok(body) = plaine_consensus::codec::BlockBody::parse(&e.body) else {
                continue;
            };
            for i in 0..body.len() {
                let Some(Ok(tx)) = body.decode_tx(i) else { continue };
                let found = match &tx {
                    plaine_consensus::codec::Tx::Coinbase(c) => c.txid().ok(),
                    plaine_consensus::codec::Tx::Transfer(t) => Some(t.txid()),
                    plaine_consensus::codec::Tx::Announcement(a) => a.txid().ok(),
                };
                if found.as_ref() == Some(txid) {
                    return Some((e.height, i as u16, body.tx_bytes(i)?.to_vec()));
                }
            }
        }
        None
    }

    pub fn addr_history(
        &self,
        addr: &[u8; 20],
        before: Option<(u64, u16)>,
        limit: usize,
    ) -> Option<plaine_storage::AddrHistory> {
        self.reader.addr_history(addr, before, limit).ok()
    }

    pub fn txindex_lookup(&self, txid: &Hash32) -> plaine_storage::TxLocation {
        match self.reader.txindex_lookup(txid) {
            Ok(l) => l,
            Err(_) => plaine_storage::TxLocation::NotIndexed { indexed_from: u64::MAX },
        }
    }

    pub fn header_raw_by_hash(&self, h: &Hash32) -> Option<(u64, [u8; HEADER_BYTES])> {
        {
            let g = self.ring.read().ok()?;
            if let Some(&i) = g.by_hash.get(h) {
                let e = &g.entries[i];
                return Some((e.height, e.header));
            }
        }
        match self.reader.header_by_hash(h) {
            Ok(v) => v,
            Err(_) => None,
        }
    }

    pub fn body_by_hash_inner(&self, h: &Hash32) -> Option<Vec<u8>> {
        {
            let g = self.ring.read().ok()?;
            if let Some(&i) = g.by_hash.get(h) {
                return Some(g.entries[i].body.clone());
            }
        }
        let (height, _) = match self.reader.header_by_hash(h) {
            Ok(Some(v)) => v,
            _ => return None,
        };
        self.body_at_verified(height)
    }
}

fn rec_from(height: u64, raw: [u8; HEADER_BYTES]) -> HeaderRec {
    let mut r = HeaderRec::from_raw(raw);
    debug_assert_eq!(r.height, height, "stored height disagrees with the header bytes");
    r.height = height;
    r
}

impl Store for NodeStore {
    fn tip(&self) -> TipRef {
        if let Ok(g) = self.ring.read() {
            if let Some(e) = g.entries.last() {
                let r = rec_from(e.height, e.header);
                let t = self.reader.tip();
                return TipRef {
                    height: e.height,
                    hash: e.hash,
                    time: r.time,
                    chainwork: plaine_consensus::rules::Work::from_be256(&t.chainwork),
                };
            }
        }
        let t = self.reader.tip();
        let time = self
            .reader
            .header_at(t.height)
            .ok()
            .flatten()
            .map(|raw| HeaderRec::from_raw(raw).time)
            .unwrap_or(0);
        TipRef {
            height: t.height,
            hash: t.hash,
            time,
            chainwork: plaine_consensus::rules::Work::from_be256(&t.chainwork),
        }
    }

    fn header_at(&self, height: u64) -> Option<HeaderRec> {
        if let Ok(g) = self.ring.read() {
            if let Some(e) = g.at(height) {
                return Some(rec_from(e.height, e.header));
            }
        }
        self.reader
            .header_at(height)
            .ok()
            .flatten()
            .map(|raw| rec_from(height, raw))
    }

    fn header_by_hash(&self, h: &Hash32) -> Option<HeaderRec> {
        if let Some((height, raw)) = self.header_raw_by_hash(h) {
            return Some(rec_from(height, raw));
        }

        match self.reader.side_header(h) {
            Ok(Some(s)) => Some(rec_from(s.height, s.header)),
            _ => None,
        }
    }

    fn hash_at(&self, height: u64) -> Option<Hash32> {
        if let Ok(g) = self.ring.read() {
            if let Some(e) = g.at(height) {
                return Some(e.hash);
            }
        }
        self.reader.hash_at(height).ok().flatten()
    }

    fn body_at(&self, height: u64) -> Option<Vec<u8>> {
        self.body_at_verified(height)
    }

    fn body_by_hash(&self, h: &Hash32) -> Option<Vec<u8>> {
        self.body_by_hash_inner(h)
    }

    fn account(&self, addr: &Address) -> Account {
        if let Ok(g) = self.ring.read() {
            for e in g.entries.iter().rev() {
                for (a, acc) in e.deltas.iter().rev() {
                    if a == addr {
                        return *acc;
                    }
                }
            }
        }
        match self.reader.account(addr) {
            Ok(a) => Account { balance: a.balance, nonce: a.nonce },
            Err(_) => Account::default(),
        }
    }

    fn accounts(&self, addrs: &[Address]) -> Vec<Account> {
        addrs.iter().map(|a| self.account(a)).collect()
    }

    fn undo_at(&self, height: u64) -> Option<Vec<UndoRec>> {
        if let Ok(g) = self.ring.read() {
            if let Some(e) = g.at(height) {
                return Some(e.undo.clone());
            }
        }
        match self.reader.undo_at(height) {
            Ok(Some(v)) => Some(
                v.into_iter()
                    .map(|u| UndoRec {
                        addr: u.addr,
                        prev_balance: u.prev_balance,
                        prev_nonce: u.prev_nonce,
                        existed: u.existed,
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    fn undo_floor(&self) -> u64 {
        self.reader.undo_floor()
    }

    fn replay_floor(&self) -> u64 {
        self.reader.replay_floor()
    }

    fn checkpoint_at_or_below(&self, height: u64) -> Option<u64> {
        self.reader.checkpoint_at_or_below(height).ok().flatten()
    }

    fn state_snapshot(&self, _height: u64) -> Option<Vec<(Address, Account)>> {
        None
    }

    fn issued(&self) -> u128 {
        let mut total = self.reader.issued();
        if let Ok(g) = self.ring.read() {
            for e in &g.entries {
                total = total.saturating_add(e.issued_delta);
            }
        }
        total
    }

    fn is_invalid(&self, h: &Hash32) -> bool {
        self.reader.is_invalid(h).unwrap_or(false)
    }

    fn side_headers_from(&self, from: u64, max: usize) -> Vec<HeaderRec> {
        self.reader
            .side_headers_from(from, max)
            .unwrap_or_default()
            .into_iter()
            .map(|s| rec_from(s.height, s.header))
            .collect()
    }

    fn headers_range(&self, from: u64, max: usize) -> Vec<[u8; HEADER_BYTES]> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        let count = max.min(plaine_consensus::constants::MAX_HEADERS_PER_MSG) as u32;
        if count > 0 {
            if let Ok(n) = self.reader.headers_range(from, count, &mut buf) {
                for i in 0..n as usize {
                    let mut h = [0u8; HEADER_BYTES];
                    h.copy_from_slice(&buf[i * HEADER_BYTES..(i + 1) * HEADER_BYTES]);
                    out.push(h);
                }
            }
        }

        if let Ok(g) = self.ring.read() {
            let mut next = from + out.len() as u64;
            while out.len() < max {
                match g.at(next) {
                    Some(e) => {
                        out.push(e.header);
                        next += 1;
                    }
                    None => break,
                }
            }
        }
        out
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CommitStats {
    pub blocks: u64,
    pub reorgs: u64,
    pub forced_seals: u64,
}

pub struct CommitSink {
    committer: Mutex<Committer>,
    ring: Unsealed,
    reader: StoreReader,
    notes: crate::wire::rpcview::NoteIndex,
    stats: Mutex<CommitStats>,
}

impl CommitSink {
    pub fn new(
        committer: Committer,
        reader: StoreReader,
        ring: Unsealed,
        notes: crate::wire::rpcview::NoteIndex,
    ) -> CommitSink {
        CommitSink {
            committer: Mutex::new(committer),
            ring,
            reader,
            notes,
            stats: Mutex::new(CommitStats::default()),
        }
    }

    pub fn stats(&self) -> CommitStats {
        *self.stats.lock().expect("commit stats")
    }

    pub fn flush(&self) -> Result<(), plaine_storage::StoreError> {
        let mut c = self.committer.lock().expect("committer");
        c.flush()?;
        drop(c);
        self.ring.write().expect("ring").prune(self.reader.hdr_watermark());
        Ok(())
    }

    fn to_plan<'a>(b: &'a CommitBlock, o: &'a Owned) -> BlockToCommit<'a> {
        BlockToCommit {
            header: &b.header_raw,
            hash: b.hash,
            height: b.height,
            body: &b.body,
            deltas: &o.deltas,
            undo: &o.undo,
            issued_delta: b.issued_delta,
            chainwork: work_be(&b.chainwork),
            txids: if o.txids.is_empty() { None } else { Some(&o.txids) },
        }
    }

    fn own(b: &CommitBlock) -> Owned {
        Owned {
            deltas: b
                .deltas
                .iter()
                .map(|d| plaine_storage::StateDelta {
                    addr: d.addr,
                    balance: d.balance,
                    nonce: d.nonce,
                })
                .collect(),
            undo: b
                .undo
                .iter()
                .map(|u| plaine_storage::UndoRec {
                    addr: u.addr,
                    prev_balance: u.prev_balance,
                    prev_nonce: u.prev_nonce,
                    existed: u.existed,
                })
                .collect(),
            txids: b.txids.clone(),
        }
    }

    fn ring_push(&self, b: &CommitBlock) {
        let mut g = self.ring.write().expect("ring");
        g.push(Entry {
            height: b.height,
            hash: b.hash,
            header: b.header_raw,
            body: b.body.clone(),
            deltas: b
                .deltas
                .iter()
                .map(|d| (d.addr, Account { balance: d.balance, nonce: d.nonce }))
                .collect(),
            undo: b.undo.clone(),
            issued_delta: b.issued_delta,
        });
    }

    fn enforce_bound(&self, c: &mut Committer) -> Result<(), plaine_storage::StoreError> {
        let (blocks, bytes) = {
            let g = self.ring.read().expect("ring");
            (g.len(), g.bytes())
        };
        // Two bounds: seal early when either the block count or the byte total trips.
        // A run of full blocks then can't pin the whole ring in ram.
        if blocks >= RING_MAX_BLOCKS || bytes >= RING_MAX_BYTES {
            c.flush()?;
            self.stats.lock().expect("stats").forced_seals += 1;
        }
        Ok(())
    }

    fn after_write(&self) {
        self.ring.write().expect("ring").prune(self.reader.hdr_watermark());
    }
}

struct Owned {
    deltas: Vec<plaine_storage::StateDelta>,
    undo: Vec<plaine_storage::UndoRec>,
    txids: Vec<Hash32>,
}

fn work_be(w: &plaine_consensus::rules::Work) -> [u8; 32] {
    let mut out = [0u8; 32];
    let limbs = w.0;
    for i in 0..4 {
        let be = limbs[i].to_be_bytes();
        let off = 24 - i * 8;
        out[off..off + 8].copy_from_slice(&be);
    }
    out
}

fn storage_err(e: plaine_storage::StoreError) -> SinkError {
    match e {
        plaine_storage::StoreError::BadPlan(d) => SinkError::Invalid(d),
        plaine_storage::StoreError::UndoExhausted { .. } => {
            SinkError::Invalid("undo ring exhausted")
        }
        plaine_storage::StoreError::ForkBelowPruneFloor { .. } => {
            SinkError::Invalid("fork below the prune floor")
        }
        _ => SinkError::Fatal("storage write failed"),
    }
}

impl Sink for CommitSink {
    fn capacity(&self) -> usize {
        1
    }

    fn commit_block(&self, b: &CommitBlock) -> Result<Receipt, SinkError> {
        let mut c = self.committer.lock().expect("committer");
        self.enforce_bound(&mut c).map_err(storage_err)?;

        self.ring_push(b);
        let owned = CommitSink::own(b);
        let plan = CommitSink::to_plan(b, &owned);
        let r = c.extend(std::slice::from_ref(&plan)).map_err(storage_err)?;
        drop(c);
        self.notes.on_block(b.height, b.hash, &b.body);
        self.after_write();
        self.stats.lock().expect("stats").blocks += 1;
        Ok(Receipt {
            tip: TipRef {
                height: r.tip.height,
                hash: r.tip.hash,
                time: HeaderRec::from_raw(b.header_raw).time,
                chainwork: b.chainwork,
            },
        })
    }

    fn commit_reorg(&self, p: &ReorgCommit) -> Result<Receipt, SinkError> {
        let mut c = self.committer.lock().expect("committer");

        // a reorg abandons the current unsealed tail. flush what is durable, drop the
        // ring, then apply the new branch.
        c.flush().map_err(storage_err)?;
        self.ring.write().expect("ring").clear();
        let fork_height = p.rollback.last().copied().map(|h| h - 1).unwrap_or_else(|| {
            p.apply.first().map(|b| b.height.saturating_sub(1)).unwrap_or(0)
        });
        let owned: Vec<Owned> = p.apply.iter().map(CommitSink::own).collect();
        let plans: Vec<BlockToCommit<'_>> = p
            .apply
            .iter()
            .zip(owned.iter())
            .map(|(b, o)| CommitSink::to_plan(b, o))
            .collect();
        let plan = ReorgPlan { fork_height, rollback: &p.rollback, apply: &plans };
        let r = c.reorg(&plan).map_err(storage_err)?;
        drop(c);
        for b in &p.apply {
            self.notes.on_block(b.height, b.hash, &b.body);
        }
        self.notes.rollback_above(fork_height);
        let mut s = self.stats.lock().expect("stats");
        s.reorgs += 1;
        s.blocks += p.apply.len() as u64;
        drop(s);
        Ok(Receipt {
            tip: TipRef {
                height: r.tip.height,
                hash: r.tip.hash,
                time: p.apply.last().map(|b| HeaderRec::from_raw(b.header_raw).time).unwrap_or(0),
                chainwork: p.apply.last().map(|b| b.chainwork).unwrap_or_default(),
            },
        })
    }

    fn commit_deep_reorg(&self, p: &DeepReorgCommit) -> Result<Receipt, SinkError> {
        let mut c = self.committer.lock().expect("committer");
        c.flush().map_err(storage_err)?;
        self.ring.write().expect("ring").clear();
        let fork_height = p.apply.first().map(|b| b.height.saturating_sub(1)).unwrap_or(p.rewind_to);
        let owned: Vec<Owned> = p.apply.iter().map(CommitSink::own).collect();
        let plans: Vec<BlockToCommit<'_>> = p
            .apply
            .iter()
            .zip(owned.iter())
            .map(|(b, o)| CommitSink::to_plan(b, o))
            .collect();
        let plan = DeepReorgPlan {
            fork_height,
            rewind_to: p.rewind_to,
            replay: &[],
            apply: &plans,
        };
        let r = c.deep_reorg(&plan).map_err(storage_err)?;
        drop(c);
        for b in &p.apply {
            self.notes.on_block(b.height, b.hash, &b.body);
        }
        self.notes.rollback_above(fork_height);
        let mut s = self.stats.lock().expect("stats");
        s.reorgs += 1;
        s.blocks += p.apply.len() as u64;
        drop(s);
        Ok(Receipt {
            tip: TipRef {
                height: r.tip.height,
                hash: r.tip.hash,
                time: p.apply.last().map(|b| HeaderRec::from_raw(b.header_raw).time).unwrap_or(0),
                chainwork: p.apply.last().map(|b| b.chainwork).unwrap_or_default(),
            },
        })
    }

    fn store_side_header(&self, h: &SideHeaderRec) -> Result<(), SinkError> {
        let mut c = self.committer.lock().expect("committer");
        let r = c
            .put_side_header(&h.rec.hash, &h.rec.raw, h.rec.height, plaine_storage::HeaderStatus::PowOk)
            .map_err(storage_err);
        drop(c);

        self.after_write();
        r
    }

    fn mark_invalid(&self, h: &Hash32) -> Result<(), SinkError> {
        let mut c = self.committer.lock().expect("committer");
        let r = c
            .mark_invalid(h, plaine_storage::InvalidReason::BadTx)
            .map_err(storage_err);
        drop(c);
        self.after_write();
        r
    }

    fn put_anchor(&self, cp: &SignedCheckpoint) -> Result<(), SinkError> {
        let raw = plaine_consensus::checkpoint_record::encode(cp);
        let mut c = self.committer.lock().expect("committer");
        let r = c.put_checkpoint_anchor(&raw).map_err(storage_err);
        drop(c);

        self.after_write();
        r
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn entry(height: u64, body: Vec<u8>) -> Entry {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&height.to_le_bytes());
        Entry {
            height,
            hash,
            header: [0u8; HEADER_BYTES],
            body,
            deltas: Vec::new(),
            undo: Vec::new(),
            issued_delta: 0,
        }
    }

    #[test]
    fn ring_answers_by_height_and_hash() {
        let mut r = UnsealedInner::default();
        r.push(entry(7, vec![1, 2, 3]));
        assert_eq!(r.at(7).map(|e| e.body.clone()), Some(vec![1, 2, 3]));
        let h = r.entries[0].hash;
        assert_eq!(r.by_hash.get(&h), Some(&0));
    }

    pub(crate) fn open_for_test(
        cfg: plaine_storage::StoreConfig,
    ) -> (plaine_storage::Committer, plaine_storage::StoreReader) {
        for _ in 0..600 {
            match plaine_storage::open(cfg.clone()) {
                Ok((c, r, _)) => return (c, r),
                Err(plaine_storage::StoreError::AlreadyOpen) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("open an empty store: {e:?}"),
            }
        }
        panic!("the store write capability was held for six seconds");
    }

    fn temp_store() -> (std::path::PathBuf, plaine_storage::Committer, plaine_storage::StoreReader)
    {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "plaine-ring-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let cfg = plaine_storage::StoreConfig::new(dir.clone(), plaine_storage::Network::Main);
        let (c, r) = open_for_test(cfg);
        (dir, c, r)
    }

    #[test]
    fn side_header_enumeration_forwarded() {
        let (dir, mut committer, reader) = temp_store();
        let ring = new_ring();
        let store = NodeStore::new(reader, Arc::clone(&ring));
        assert_eq!(store.side_headers_from(0, 16).len(), 0, "an empty store has no side headers");

        let mut rows = Vec::new();
        for h in 1u64..=3 {
            let mut hdr = [0u8; HEADER_BYTES];
            hdr[4..12].copy_from_slice(&h.to_le_bytes());
            let mut hash = [0u8; 32];
            hash[0] = h as u8;
            rows.push((hash, hdr, h, plaine_storage::HeaderStatus::Connected));
        }
        committer.put_side_headers(&rows).expect("side headers");
        committer.flush().expect("flush");

        let got = store.side_headers_from(0, 16);
        assert_eq!(
            got.len(),
            3,
            "the arena rebuild reads nothing, so only the one-link repair runs"
        );

        assert!(
            got.windows(2).all(|w| w[0].height <= w[1].height),
            "side headers came back out of height order"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tx_in_ring_finds_unsealed_tx() {
        use plaine_consensus::codec::{AuthorNote, BlockBody, CoinbaseTx, Tx};
        let (_dir, _committer, reader) = temp_store();
        let ring = new_ring();
        let store = NodeStore::new(reader, Arc::clone(&ring));

        let cb = CoinbaseTx {
            height: 5,
            to: [0x77; 20],
            reward: plaine_consensus::emission::block_reward(5),
            fees: 0,
            note: AuthorNote { encoding: 0x01, payload: vec![0x41; 3] },
        };
        let rec = cb.encode().expect("encode");
        let body = BlockBody::encode(&[rec.as_slice()]).expect("body");
        let txid = match BlockBody::parse(&body).unwrap().decode_tx(0).unwrap().unwrap() {
            Tx::Coinbase(c) => c.txid().unwrap(),
            _ => unreachable!(),
        };

        assert!(store.body_at(5).is_none(), "the reader must be empty for this to prove anything");
        assert!(store.tx_in_ring(&txid).is_none());

        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&5u64.to_le_bytes());
        ring.write().expect("ring").push(Entry {
            height: 5,
            hash,
            header: [0u8; HEADER_BYTES],
            body,
            deltas: Vec::new(),
            undo: Vec::new(),
            issued_delta: 0,
        });

        let (h, ix, raw) = store
            .tx_in_ring(&txid)
            .expect("tx_in_ring must find a tx whose block is still unsealed");
        assert_eq!(h, 5);
        assert_eq!(ix, 0);
        assert_eq!(raw, rec, "the bytes must be the record's own, not a re-serialization");

        assert!(store.tx_in_ring(&[0xEE; 32]).is_none());
    }

    #[test]
    fn all_reads_consult_the_ring() {
        let (dir, _committer, reader) = temp_store();
        let ring = new_ring();
        let store = NodeStore::new(reader, Arc::clone(&ring));

        assert!(store.header_at(0).is_none(), "the empty store must not answer for height 0");
        assert!(store.body_at(0).is_none());
        assert!(store.hash_at(0).is_none());
        assert!(store.undo_at(0).is_none());
        assert_eq!(store.headers_range(0, 8).len(), 0);

        let addr: Address = [7u8; 20];
        let funded = Account { balance: 4_242, nonce: 9 };
        let mut header = [0u8; HEADER_BYTES];
        header[0] = 0xAB;
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&0u64.to_le_bytes());
        ring.write().expect("ring").push(Entry {
            height: 0,
            hash,
            header,
            body: vec![0xC0, 0xFF, 0xEE],
            deltas: vec![(addr, funded)],
            undo: vec![UndoRec { addr, prev_balance: 1, prev_nonce: 2, existed: true }],
            issued_delta: 500,
        });

        assert_eq!(
            store.header_at(0).map(|r| r.raw[0]),
            Some(0xAB),
            "header_at lost its ring read: the validator cannot see a block it just committed"
        );
        assert_eq!(store.hash_at(0), Some(hash), "hash_at lost its ring read");
        assert_eq!(
            store.body_at(0),
            Some(vec![0xC0, 0xFF, 0xEE]),
            "body_at lost its ring read - the coinbase-maturity ledger depends on it"
        );
        assert_eq!(
            store.header_by_hash(&hash).map(|r| r.raw[0]),
            Some(0xAB),
            "header_by_hash lost its ring read"
        );
        assert_eq!(
            store.body_by_hash(&hash),
            Some(vec![0xC0, 0xFF, 0xEE]),
            "body_by_hash lost its ring read - have_body is hash-keyed"
        );
        assert_eq!(
            store.undo_at(0).map(|v| v.len()),
            Some(1),
            "undo_at lost its ring read - a missing undo row forces a needless deep replay"
        );
        assert_eq!(store.account(&addr), funded, "account lost its ring read");
        assert_eq!(store.accounts(&[addr]), vec![funded], "accounts lost its ring read");
        assert_eq!(store.issued(), 500, "issued lost its ring read");
        assert_eq!(
            store.headers_range(0, 8).len(),
            1,
            "headers_range lost its unsealed tail and stops short of the announced tip"
        );

        ring.write().expect("ring").clear();
        assert!(store.header_at(0).is_none(), "a cleared ring must stop answering");
        assert_eq!(store.issued(), 0, "issued must stop double-counting a sealed block");

        drop(_committer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_drops_sealed_heights() {
        let mut r = UnsealedInner::default();
        for h in 0..10 {
            r.push(entry(h, vec![0; 100]));
        }
        assert_eq!(r.len(), 10);
        r.prune(4);
        assert_eq!(r.len(), 6);
        assert!(r.at(3).is_none(), "3 is below the watermark and is the reader's now");
        assert!(r.at(4).is_some());
        assert_eq!(r.bytes(), 600, "byte accounting survives a prune");

        let h = r.entries[0].hash;
        assert_eq!(r.by_hash.get(&h), Some(&0));
    }

    #[test]
    fn byte_bound_trips_before_block_bound() {
        let mut r = UnsealedInner::default();
        let mut n = 0;
        while r.bytes() < RING_MAX_BYTES {
            r.push(entry(n, vec![0u8; plaine_storage::MAX_BODY_BYTES]));
            n += 1;
        }
        assert!(
            r.len() < RING_MAX_BLOCKS,
            "the byte bound must trip first at MAX_BODY_BYTES, or 256 full blocks \
             (~256 MiB) sit in RAM against a 512 MiB target"
        );
    }

    #[test]
    fn newest_entry_wins_in_window() {
        let mut r = UnsealedInner::default();
        r.push(entry(5, vec![0xAA]));
        r.push(entry(5, vec![0xBB]));
        assert_eq!(r.at(5).map(|e| e.body.clone()), Some(vec![0xBB]));
    }

    #[test]
    fn l2_sweep_unjudged_not_verified() {
        let (_dir, _c, reader) = temp_store();
        let r = crate::node::l2_frame_sweep(&reader);
        assert_eq!(
            r.verified, 0,
            "a store with nothing sealed has verified nothing: {r:?}"
        );
        assert!(r.mismatched.is_empty(), "{r:?}");
        assert!(
            r.unjudged > 0,
            "every segment of a fresh store answers Ok(None), which must be \
             counted: {r:?}"
        );
        let line = r.line();
        assert!(
            line.contains("could not be judged"),
            "the operator must be told the difference in words: {line}"
        );
        assert!(
            line.contains("is not") && line.contains("clean"),
            "and told that it is not the same as clean: {line}"
        );

        assert!(line.contains("0 verified"), "{line}");
        assert!(line.contains("0 MISMATCHED"), "{line}");
    }

}
