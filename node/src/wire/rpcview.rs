use std::sync::{Arc, Mutex, RwLock};

use plaine_rpc::views::{
    AccountRecord, Address20, AuthorKeyStatus, AuthorNote, AuthorNotesPage, BlockRecord, Budgets,
    ChainInfo, CheckpointStatus, CheckpointSubmit, EmissionAudit, FeeSuggestion, HeaderRecord,
    Direction, Hash32, HistoryEntry, HistoryKind, HistoryLookup, MempoolInfo,
    Network, NotesCursor, PeerInfo, StratumSession, SubmitError, SyncStatus, TxLocation, TxLookup,
    TxRecord, Verbosity,
};

use plaine_chain::error::Reject;
use plaine_consensus::tx::TxError;

use crate::validator::{Cmd, Query};
use crate::wire::store::NodeStore;
use crate::wire::tip::TipCell;

#[derive(Clone, Debug)]
struct Note {
    seq: u64,
    height: u64,
    txid: Hash32,
    time: u64,
    encoding: u8,
    payload: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct NoteIndex(Arc<RwLock<Vec<Note>>>);

impl NoteIndex {
    pub fn new() -> NoteIndex {
        NoteIndex(Arc::new(RwLock::new(Vec::new())))
    }

    pub fn on_block(&self, height: u64, _hash: Hash32, body: &[u8]) {
        let Ok(b) = plaine_consensus::codec::BlockBody::parse(body) else {
            return;
        };
        let time = 0u64;
        let mut g = self.0.write().expect("note index");
        for i in 0..b.len() {
            let Some(Ok(plaine_consensus::codec::Tx::Announcement(a))) = b.decode_tx(i) else {
                continue;
            };
            let Ok(txid) = a.txid() else { continue };
            let seq = g.len() as u64;
            g.push(Note {
                seq,
                height,
                txid,
                time,
                encoding: a.encoding,
                payload: a.payload.clone(),
            });
        }
    }

    // Drop notes from blocks a reorg orphaned, then recompact seq so paging stays
    // contiguous. seq is a position, not a stable id. The renumber is O(notes), but
    // it only runs on a reorg - cold enough not to care.
    pub fn rollback_above(&self, fork_height: u64) {
        let mut g = self.0.write().expect("note index");
        g.retain(|n| n.height <= fork_height);
        for (i, n) in g.iter_mut().enumerate() {
            n.seq = i as u64;
        }
    }

    /// A reorg onto `fork_height`: drop the notes of the orphaned blocks, then add
    /// those of the applied ones. Adding first would have the rollback drop the new
    /// branch's notes as well, since they sit above the fork too.
    pub fn on_reorg<'a>(
        &self,
        fork_height: u64,
        applied: impl IntoIterator<Item = (u64, Hash32, &'a [u8])>,
    ) {
        self.rollback_above(fork_height);
        for (height, hash, body) in applied {
            self.on_block(height, hash, body);
        }
    }

    fn page(&self, cursor: NotesCursor, limit: usize, tip: u64) -> AuthorNotesPage {
        let g = self.0.read().expect("note index");
        let total = g.len() as u64;
        let filtered: Vec<&Note> = g
            .iter()
            .rev()
            .filter(|n| match cursor {
                NotesCursor::Newest => true,
                NotesCursor::AtOrBelowHeight(h) => n.height <= h,
                NotesCursor::Before(s) => n.seq < s,
            })
            .collect();
        let notes: Vec<AuthorNote> = filtered
            .iter()
            .take(limit)
            .map(|n| AuthorNote {
                seq: n.seq,
                height: n.height,
                txid: n.txid,
                time: n.time,
                confirmations: tip.saturating_sub(n.height) + 1,
                encoding: n.encoding,
                payload: n.payload.clone(),
            })
            .collect();
        let more = filtered.len() > notes.len();
        let next_seq = if more { notes.last().map(|n| n.seq) } else { None };
        AuthorNotesPage { notes, more, total, next_seq }
    }
}

#[derive(Clone)]
pub struct Ask {
    tx: tokio::sync::mpsc::Sender<Cmd>,
    refused: Arc<std::sync::atomic::AtomicU64>,
}

impl Ask {
    pub fn new(tx: tokio::sync::mpsc::Sender<Cmd>) -> Ask {
        Ask { tx, refused: Arc::new(std::sync::atomic::AtomicU64::new(0)) }
    }

    pub fn queue_depth(&self) -> usize {
        self.tx.max_capacity().saturating_sub(self.tx.capacity())
    }

    pub fn queue_capacity(&self) -> usize {
        self.tx.max_capacity()
    }

    pub fn refused(&self) -> u64 {
        self.refused.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn ask<T: Send + 'static>(
        &self,
        make: impl FnOnce(std::sync::mpsc::SyncSender<T>) -> Query,
        wait: std::time::Duration,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.try_send(Cmd::Ask(make(tx))).is_err() {
            self.refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        match rx.recv_timeout(wait) {
            Ok(v) => Some(v),
            Err(_) => {
                self.refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                None
            }
        }
    }
}

const ASK_DEADLINE: std::time::Duration = std::time::Duration::from_millis(2_000);

pub struct RpcChain {
    pub tip: TipCell,
    pub store: Arc<NodeStore>,
    pub notes: NoteIndex,
    pub ask: Ask,
    pub network: Network,
    pub pruned: bool,
    pub txindex: bool,
    pub best_known: Arc<std::sync::atomic::AtomicU64>,
    pub peers: Arc<std::sync::atomic::AtomicUsize>,
    pub verdict: Arc<std::sync::Mutex<Option<crate::health::Latched>>>,
}

impl RpcChain {
    fn header_record(&self, r: plaine_chain::types::HeaderRec) -> HeaderRecord {
        use plaine_chain::traits::Store;
        let tip = self.tip.get();
        let canonical = self.store.hash_at(r.height) == Some(r.hash);
        HeaderRecord {
            raw: r.raw,
            hash: r.hash,
            height: r.height,
            confirmations: if canonical { tip.height.saturating_sub(r.height) + 1 } else { 0 },
            canonical,
            chainwork: if canonical && r.height == tip.height { tip.chainwork } else { [0u8; 32] },
        }
    }

    fn block_record(&self, r: plaine_chain::types::HeaderRec, v: Verbosity) -> Option<BlockRecord> {
        let body = self.store.body_at_verified(r.height)?;
        let parsed = plaine_consensus::codec::BlockBody::parse(&body).ok()?;
        let header = self.header_record(r);
        let author_note = match parsed.decode_tx(0) {
            Some(Ok(plaine_consensus::codec::Tx::Coinbase(cb))) => cb.note.payload.clone(),
            _ => Vec::new(),
        };
        let mut txids = Vec::new();
        if v != Verbosity::RawHex {
            for i in 0..parsed.len() {
                if let Some(Ok(tx)) = parsed.decode_tx(i) {
                    if let Some(id) = txid_of(&tx) {
                        txids.push(id);
                    }
                }
            }
        }
        let raw = if v == Verbosity::RawHex {
            let mut all = Vec::with_capacity(132 + body.len());
            all.extend_from_slice(&header.raw);
            all.extend_from_slice(&body);
            Some(all)
        } else {
            None
        };
        Some(BlockRecord {
            tx_count: parsed.len(),
            size_bytes: 132 + body.len(),
            author_note,
            raw,
            txids,
            txs: Vec::new(),
            header,
        })
    }
}

fn verify_txid_hint(
    body: &plaine_consensus::codec::BlockBody,
    txid: &Hash32,
    hint: u16,
) -> Option<usize> {
    // the stored index is only a hint. try it first, wrap around the rest of the
    // block, and confirm the txid matches before trusting it.
    let hint = (hint as usize).min(body.len());
    (hint..body.len()).chain(0..hint).find(|&i| {
        body.decode_tx(i)
            .and_then(|r| r.ok())
            .and_then(|t| txid_of(&t))
            .as_ref()
            == Some(txid)
    })
}

fn txid_of(tx: &plaine_consensus::codec::Tx) -> Option<Hash32> {
    use plaine_consensus::codec::Tx;
    match tx {
        Tx::Coinbase(c) => c.txid().ok(),
        Tx::Transfer(t) => Some(t.txid()),
        Tx::Announcement(a) => a.txid().ok(),
    }
}

impl plaine_rpc::views::ChainView for RpcChain {
    fn info(&self) -> ChainInfo {
        let t = self.tip.get();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let tip_age = now.saturating_sub(t.time);
        let peers = self.peers.load(std::sync::atomic::Ordering::Relaxed);
        let best = self.best_known.load(std::sync::atomic::Ordering::Relaxed);

        let best_known_height = crate::health::best_known(peers, best, t.height);

        // A halt beats every other verdict. Otherwise serve the heartbeat's latched
        // verdict, refreshed against live height and peers - never a fresh assessment
        // of our own. That is what keeps the rpc and the log from contradicting.
        let (sync, stall_reason) = if let Some(h) = t.halted {
            (SyncStatus::Stalled, Some(format!("the validator has halted: {h}")))
        } else {
            match self.verdict.lock().expect("verdict cell").as_ref() {
                Some(l) => {
                    let v = l.refreshed(crate::health::Live {
                        height: t.height,
                        tip_age_secs: tip_age,
                        peers,
                        best_known_height,
                    });
                    (v.status, v.reason)
                }

                None => (SyncStatus::Starting, None),
            }
        };
        ChainInfo {
            network: self.network,
            version: format!("plaine-noded/{}", env!("CARGO_PKG_VERSION")),
            height: t.height,
            tip_hash: t.hash,
            chainwork: t.chainwork,
            tip_time: t.time,
            tip_age_secs: tip_age,
            sync,
            best_known_height,
            stall_reason,
            txindex: self.txindex,
            pruned: self.pruned,
            prune_horizon_height: self.store.reader().prune_floor(),
        }
    }

    fn header_by_height(&self, height: u64) -> Option<HeaderRecord> {
        use plaine_chain::traits::Store;
        self.store.header_at(height).map(|r| self.header_record(r))
    }

    fn invalid_reason(&self, hash: &Hash32) -> Option<String> {
        self.store.invalid_reason(hash).map(|r| r.as_str().to_string())
    }

    fn header_by_hash(&self, hash: &Hash32) -> Option<HeaderRecord> {
        use plaine_chain::traits::Store;
        self.store.header_by_hash(hash).map(|r| self.header_record(r))
    }

    fn block_by_height(&self, height: u64, verbosity: Verbosity) -> Option<BlockRecord> {
        use plaine_chain::traits::Store;
        let r = self.store.header_at(height)?;
        self.block_record(r, verbosity)
    }

    fn block_by_hash(&self, hash: &Hash32, verbosity: Verbosity) -> Option<BlockRecord> {
        use plaine_chain::traits::Store;
        let r = self.store.header_by_hash(hash)?;
        // Bodies are kept by height for the best chain only. A side-branch header
        // has no body here: its height holds another block's.
        if self.store.hash_at(r.height) != Some(*hash) {
            return None;
        }
        self.block_record(r, verbosity)
    }

    fn account(&self, addr: &Address20) -> AccountRecord {
        use plaine_chain::traits::Store;
        let a = self.store.account(addr);

        // coinbase credits still inside the maturity window; these only count toward
        // the balance once they age past it.
        let tip = self.tip.get().height;
        let lo = tip.saturating_sub(plaine_consensus::constants::COINBASE_MATURITY - 1);
        let mut immature: u128 = 0;
        for h in lo..=tip {
            let Some(body) = self.store.body_at_verified(h) else { continue };
            let Ok(b) = plaine_consensus::codec::BlockBody::parse(&body) else { continue };
            if let Some(Ok(plaine_consensus::codec::Tx::Coinbase(cb))) = b.decode_tx(0) {
                if cb.to == *addr {
                    if let Ok(c) = plaine_consensus::tx::coinbase_credit(&cb) {
                        immature = immature.saturating_add(c);
                    }
                }
            }
        }
        let pending = self
            .ask
            .ask(|r| Query::BySender(*addr, r), ASK_DEADLINE)
            .map(|v| v.len() as u64)
            .unwrap_or(0);
        AccountRecord {
            balance: a.balance,
            nonce: a.nonce,
            pending_nonce: a.nonce + pending,
            immature,
        }
    }

    fn tx(&self, txid: &Hash32) -> TxLookup {
        if let Some(Some(raw)) = self.ask.ask(|r| Query::Tx(*txid, r), ASK_DEADLINE) {
            if let Ok(tx) = plaine_consensus::codec::decode_tx(&raw) {
                return TxLookup::Found(TxRecord {
                    txid: *txid,
                    type_byte: tx.type_byte(),
                    raw,
                    location: TxLocation::Mempool,
                    decoded: plaine_rpc::json::Json::Null,
                });
            }
        }
        let tip = self.tip.get().height;
        let confirmed = |height: u64, raw: Vec<u8>| -> Option<TxLookup> {
            let tx = plaine_consensus::codec::decode_tx(&raw).ok()?;
            Some(TxLookup::Found(TxRecord {
                txid: *txid,
                type_byte: tx.type_byte(),
                raw,
                location: TxLocation::Block {
                    height,
                    confirmations: tip.saturating_sub(height) + 1,
                },
                decoded: plaine_rpc::json::Json::Null,
            }))
        };
        if let Some((height, _index, raw)) = self.store.tx_in_ring(txid) {
            if let Some(found) = confirmed(height, raw) {
                return found;
            }
        }
        if !self.txindex {
            return TxLookup::NotIndexed { indexed_from: None };
        }
        match self.store.txindex_lookup(txid) {
            plaine_storage::TxLocation::NotIndexed { indexed_from } => TxLookup::NotIndexed {
                indexed_from: if indexed_from == u64::MAX { None } else { Some(indexed_from) },
            },
            plaine_storage::TxLocation::Absent => TxLookup::Absent,
            plaine_storage::TxLocation::Found { height, index } => {
                let Some(body_bytes) = self.store.body_at_verified(height) else {
                    return TxLookup::Pruned { height };
                };
                let Ok(body) = plaine_consensus::codec::BlockBody::parse(&body_bytes) else {
                    return TxLookup::Absent;
                };
                let hit = verify_txid_hint(&body, txid, index);
                match hit.and_then(|i| body.tx_bytes(i)) {
                    Some(raw) => confirmed(height, raw.to_vec()).unwrap_or(TxLookup::Absent),
                    None => TxLookup::Absent,
                }
            }
        }
    }

    fn emission_audit(&self, height: u64) -> Option<EmissionAudit> {
        use plaine_chain::traits::Store;
        if height > self.tip.get().height {
            return None;
        }
        Some(EmissionAudit {
            height,
            issued_mile: self.store.issued(),
            expected_by_formula_mile: plaine_consensus::emission::issued_through(height),
            max_supply_mile: None,
            subsidy_at_height_mile: plaine_consensus::emission::block_reward(height),
        })
    }

    fn author_notes(&self, cursor: NotesCursor, limit: usize) -> AuthorNotesPage {
        self.notes.page(cursor, limit, self.tip.get().height)
    }

    // The index hands out positions. Each one is checked against the body that is
    // canonical at its height now: a row left behind by a block that a reorg
    // replaced names a transaction that no longer touches the address, and is
    // skipped - the same approach tx() takes with txindex hits.
    fn account_history(
        &self,
        addr: &Address20,
        before: Option<(u64, u16)>,
        limit: usize,
    ) -> HistoryLookup {
        let tip = self.tip.get().height;
        let mut entries: Vec<HistoryEntry> = Vec::with_capacity(limit);
        let mut cursor = before;
        let mut unavailable_below: Option<u64> = None;
        let mut body: Option<(u64, Vec<u8>)> = None;
        let mut time: Option<(u64, u64)> = None;
        loop {
            // A few more than still needed, since stale rows drop out below.
            let want = limit - entries.len() + 8;
            let (indexed_from, hits, more) = match self.store.addr_history(addr, cursor, want) {
                None | Some(plaine_storage::AddrHistory::NotIndexed) => {
                    return HistoryLookup::NotIndexed
                }
                Some(plaine_storage::AddrHistory::Page { indexed_from, hits, more }) => {
                    (indexed_from, hits, more)
                }
            };
            for (n, hit) in hits.iter().enumerate() {
                cursor = Some((hit.height, hit.index));
                if hit.height > tip {
                    continue;
                }
                if body.as_ref().map(|b| b.0) != Some(hit.height) {
                    match self.store.body_at_verified(hit.height) {
                        Some(raw) => body = Some((hit.height, raw)),
                        None => {
                            body = None;
                            let above = hit.height + 1;
                            unavailable_below = Some(unavailable_below.map_or(above, |u| u.max(above)));
                            continue;
                        }
                    }
                }
                let Some((_, raw)) = body.as_ref() else { continue };
                let Ok(parsed) = plaine_consensus::codec::BlockBody::parse(raw) else { continue };
                let Some(Ok(tx)) = parsed.decode_tx(hit.index as usize) else { continue };
                let Some((kind, direction, amount_mile, fee_mile, counterparty)) = describe(&tx, addr)
                else {
                    continue;
                };
                let Some(txid) = txid_of(&tx) else { continue };
                if time.map(|t| t.0) != Some(hit.height) {
                    let t = self
                        .header_by_height(hit.height)
                        .and_then(|h| plaine_consensus::codec::Header::decode(&h.raw).ok())
                        .map(|h| h.time)
                        .unwrap_or(0);
                    time = Some((hit.height, t));
                }
                entries.push(HistoryEntry {
                    txid,
                    height: hit.height,
                    index: hit.index,
                    time: time.map(|t| t.1).unwrap_or(0),
                    confirmations: tip - hit.height + 1,
                    kind,
                    direction,
                    amount_mile,
                    fee_mile,
                    counterparty,
                });
                if entries.len() == limit {
                    let rest = n + 1 < hits.len() || more;
                    return HistoryLookup::Page {
                        indexed_from,
                        entries,
                        next_cursor: rest.then_some((hit.height, hit.index)),
                        unavailable_below,
                    };
                }
            }
            if !more {
                return HistoryLookup::Page { indexed_from, entries, next_cursor: None, unavailable_below };
            }
        }
    }

    fn tx_via_history(&self, txid: &Hash32, addr: &Address20) -> Option<TxLookup> {
        let tip = self.tip.get().height;
        let mut cursor = None;
        let mut searched = 0usize;
        loop {
            let (indexed_from, hits, more) = match self.store.addr_history(addr, cursor, 256)? {
                plaine_storage::AddrHistory::NotIndexed => return None,
                plaine_storage::AddrHistory::Page { indexed_from, hits, more } => {
                    (indexed_from, hits, more)
                }
            };
            for hit in &hits {
                cursor = Some((hit.height, hit.index));
                if hit.height > tip {
                    continue;
                }
                // Each hit costs a body read; past the bound, say how far the search
                // got rather than keep the RPC worker busy.
                searched += 1;
                let Some(raw) = (searched <= TX_SEARCH_MAX_HITS)
                    .then(|| self.store.body_at_verified(hit.height))
                    .flatten()
                else {
                    return Some(TxLookup::NotIndexed { indexed_from: Some(hit.height + 1) });
                };
                let Ok(body) = plaine_consensus::codec::BlockBody::parse(&raw) else { continue };
                let i = hit.index as usize;
                let Some(Ok(tx)) = body.decode_tx(i) else { continue };
                if txid_of(&tx).as_ref() != Some(txid) {
                    continue;
                }
                let Some(tx_raw) = body.tx_bytes(i) else { continue };
                return Some(TxLookup::Found(TxRecord {
                    txid: *txid,
                    type_byte: tx.type_byte(),
                    raw: tx_raw.to_vec(),
                    location: TxLocation::Block {
                        height: hit.height,
                        confirmations: tip - hit.height + 1,
                    },
                    decoded: plaine_rpc::json::Json::Null,
                }));
            }
            if !more {
                return Some(if indexed_from == 0 {
                    TxLookup::Absent
                } else {
                    TxLookup::NotIndexed { indexed_from: Some(indexed_from) }
                });
            }
        }
    }
}

/// How many of an address's index hits `tx_via_history` checks before giving up.
/// A wallet looks for its own recent transactions, which come first.
const TX_SEARCH_MAX_HITS: usize = 10_000;

/// How `tx` looks from `addr`'s side, or `None` when it does not touch `addr`.
fn describe(
    tx: &plaine_consensus::codec::Tx,
    addr: &Address20,
) -> Option<(HistoryKind, Direction, u128, u128, Option<Address20>)> {
    use plaine_consensus::codec::Tx;
    use plaine_consensus::crypto::address_payload;
    match tx {
        Tx::Coinbase(cb) if cb.to == *addr => {
            let credit = plaine_consensus::tx::coinbase_credit(cb).unwrap_or(cb.reward);
            Some((HistoryKind::Coinbase, Direction::In, credit, 0, None))
        }
        Tx::Coinbase(_) => None,
        Tx::Transfer(t) => {
            let from = address_payload(&t.from_pub);
            let (direction, peer) = match (from == *addr, t.to == *addr) {
                (true, true) => (Direction::SelfTransfer, *addr),
                (true, false) => (Direction::Out, t.to),
                (false, true) => (Direction::In, from),
                (false, false) => return None,
            };
            Some((HistoryKind::Transfer, direction, t.amount, t.fee, Some(peer)))
        }
        Tx::Announcement(a) if address_payload(&a.from_pub) == *addr => {
            Some((HistoryKind::Announcement, Direction::Out, 0, a.fee, None))
        }
        Tx::Announcement(_) => None,
    }
}

pub struct RpcMempool {
    pub ask: Ask,
    pub tx: tokio::sync::mpsc::Sender<Cmd>,
    pub relay_fee_mile: u128,
    pub max_txs: usize,
    pub store: Arc<NodeStore>,
    pub tip: TipCell,
    /// The last suggestion and the tip it was computed at; recomputed when the tip moves.
    pub fee_cache: Mutex<Option<(Hash32, FeeSuggestion)>>,
}

/// Blocks `fee_suggest` samples: SPEC §14's "last 240 blocks", four hours at 60 s.
pub const FEE_SAMPLE_BLOCKS: u64 = 240;

/// The fees of the transfers in the `count` blocks up to `tip`, and how many blocks
/// were read. Stops at the first body the store no longer holds (a pruned node).
fn sample_transfer_fees(store: &NodeStore, tip: u64, count: u64) -> (u64, Vec<u128>) {
    let mut fees = Vec::new();
    let mut blocks = 0;
    for h in (tip.saturating_sub(count.saturating_sub(1))..=tip).rev() {
        let Some(raw) = store.body_at_verified(h) else { break };
        blocks += 1;
        let Ok(body) = plaine_consensus::codec::BlockBody::parse(&raw) else { continue };
        for i in 0..body.len() {
            if let Some(Ok(plaine_consensus::codec::Tx::Transfer(t))) = body.decode_tx(i) {
                fees.push(t.fee);
            }
        }
    }
    (blocks, fees)
}

/// Nearest-rank percentiles of the sampled fees, never below the relay floor: a fee
/// this node would refuse is no suggestion. With no transfers sampled, the floor.
fn suggest_from(blocks: u64, fees: &mut [u128], floor: u128) -> FeeSuggestion {
    fees.sort_unstable();
    let pick = |p: usize| -> u128 {
        if fees.is_empty() {
            return floor;
        }
        let rank = (p * fees.len()).div_ceil(100).max(1);
        fees[rank - 1].max(floor)
    };
    FeeSuggestion {
        blocks_sampled: blocks,
        p10_mile: pick(10),
        p50_mile: pick(50),
        p90_mile: pick(90),
        relay_floor_mile: floor,
    }
}

impl plaine_rpc::views::MempoolView for RpcMempool {
    fn info(&self) -> MempoolInfo {
        let s = self.ask.ask(Query::MempoolInfo, ASK_DEADLINE).unwrap_or_default();
        MempoolInfo {
            tx_count: s.tx_count,
            bytes: s.bytes,
            executable: s.executable,
            queued: s.queued,
            relay_fee_mile: self.relay_fee_mile,
            max_txs: self.max_txs,
        }
    }

    fn by_sender(&self, addr: &Address20) -> Vec<TxRecord> {
        let raws = self.ask.ask(|r| Query::BySender(*addr, r), ASK_DEADLINE).unwrap_or_default();
        raws.into_iter()
            .filter_map(|raw| {
                let tx = plaine_consensus::codec::decode_tx(&raw).ok()?;
                Some(TxRecord {
                    txid: txid_of(&tx)?,
                    type_byte: tx.type_byte(),
                    raw,
                    location: TxLocation::Mempool,
                    decoded: plaine_rpc::json::Json::Null,
                })
            })
            .collect()
    }

    fn fee_suggest(&self) -> FeeSuggestion {
        let tip = self.tip.get();
        if let Some((at, s)) = self.fee_cache.lock().ok().and_then(|g| *g) {
            if at == tip.hash {
                return s;
            }
        }
        let (blocks, mut fees) = sample_transfer_fees(&self.store, tip.height, FEE_SAMPLE_BLOCKS);
        let s = suggest_from(blocks, &mut fees, self.relay_fee_mile);
        if let Ok(mut g) = self.fee_cache.lock() {
            *g = Some((tip.hash, s));
        }
        s
    }

    fn submit(&self, raw: &[u8]) -> Result<Hash32, SubmitError> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);

        self.tx
            .try_send(Cmd::Tx {
                origin: plaine_chain::types::TxOrigin::Local,
                bytes: raw.to_vec(),
                reply: Some(reply),
            })
            .map_err(|_| SubmitError::NotReady)?;
        match rx.recv_timeout(ASK_DEADLINE) {
            Ok(Ok(txid)) => Ok(txid),
            Ok(Err(why)) => Err(map_reject(&why)),
            Err(_) => Err(SubmitError::NotReady),
        }
    }
}

fn map_reject(why: &Reject) -> SubmitError {
    use plaine_chain::error::SubmitTag;
    match (why.submit_tag(), why) {
        (SubmitTag::NotAuthorKey, _) => SubmitError::NotAuthorKey,
        (SubmitTag::BadSignature, _) => SubmitError::BadSignature,
        (SubmitTag::Duplicate, _) => SubmitError::Duplicate,
        (SubmitTag::NotReady, _) => SubmitError::NotReady,

        (SubmitTag::TooLarge, Reject::TxTooLarge { got }) => SubmitError::TooLarge {
            len: *got,
            max: plaine_consensus::constants::MAX_TX_BYTES,
        },
        (SubmitTag::TooLarge, _) => SubmitError::TooLarge {
            len: 0,
            max: plaine_consensus::constants::MAX_TX_BYTES,
        },

        (SubmitTag::FeeBelowFloor, Reject::BelowRelayFloor { fee, floor }) => {
            SubmitError::FeeBelowRelayFloor { fee: *fee, floor: *floor }
        }
        (SubmitTag::FeeBelowFloor, Reject::Tx { err: TxError::FeeBelowFloor { fee }, .. }) => {
            SubmitError::FeeBelowRelayFloor {
                fee: *fee,
                floor: plaine_consensus::constants::FEE_FLOOR_MILE,
            }
        }
        (SubmitTag::FeeBelowFloor, _) => SubmitError::FeeBelowRelayFloor {
            fee: 0,
            floor: plaine_consensus::constants::FEE_FLOOR_MILE,
        },

        (SubmitTag::NonceOutOfRange, r) => {
            let gap = plaine_consensus::constants::MAX_MEMPOOL_NONCE_GAP;
            match r {
                Reject::TxStale { next, got } | Reject::NonceGapTooLarge { next, got } => {
                    SubmitError::NonceOutOfRange { got: *got, next: *next, max_gap: gap }
                }
                Reject::Tx { err: TxError::BadNonce { expected, got }, .. } => {
                    SubmitError::NonceOutOfRange { got: *got, next: *expected, max_gap: gap }
                }
                _ => SubmitError::NonceOutOfRange { got: 0, next: 0, max_gap: gap },
            }
        }

        (SubmitTag::InsufficientFunds, Reject::InsufficientBalance { need, have, .. }) => {
            SubmitError::InsufficientFunds { need: *need, have: *have }
        }
        (
            SubmitTag::InsufficientFunds,
            Reject::Tx { err: TxError::InsufficientBalance { need, have }, .. },
        ) => SubmitError::InsufficientFunds { need: *need, have: *have },
        (SubmitTag::InsufficientFunds, _) => SubmitError::InsufficientFunds { need: 0, have: 0 },

        (SubmitTag::ReplacementUnderpriced, Reject::ReplacementUnderpriced { need, got }) => {
            SubmitError::ReplacementUnderpriced { need: *need, got: *got }
        }
        (SubmitTag::ReplacementUnderpriced, _) => {
            SubmitError::ReplacementUnderpriced { need: 0, got: 0 }
        }

        (SubmitTag::PoolFull, Reject::SenderCap { cap }) => SubmitError::PoolFull { cap: *cap },
        (SubmitTag::PoolFull, _) => {
            SubmitError::PoolFull { cap: plaine_consensus::constants::MAX_MEMPOOL_TXS }
        }

        (SubmitTag::Malformed, _) => SubmitError::Malformed(
            "the bytes do not decode as a transfer or an author announcement, or carry a type \
             byte RPC does not accept"
                .to_string(),
        ),
    }
}

pub struct RpcNet {
    pub peers: Arc<Mutex<Vec<PeerInfo>>>,
}

impl plaine_rpc::views::NetView for RpcNet {
    fn peers(&self) -> Vec<PeerInfo> {
        self.peers.lock().expect("peer snapshot").clone()
    }
}

pub struct RpcStratum {
    pub server: Option<Arc<plaine_stratum::server::StratumServer>>,
}

impl plaine_rpc::views::StratumView for RpcStratum {
    fn sessions(&self) -> Vec<StratumSession> {
        let Some(srv) = self.server.as_ref() else {
            return Vec::new();
        };
        // one clock read for the whole snapshot, so every age is measured against
        // the same instant. cards carry monotonic-ms stamps from this same server.
        let now = srv.now_ms();
        srv.session_cards()
            .into_iter()
            .map(|c| {
                let last_share_secs = if c.had_share {
                    (now.saturating_sub(c.last_share_ms) / 1000) as i64
                } else {
                    -1
                };
                StratumSession {
                    worker: c.worker,
                    address: c.address,
                    rig: c.rig,
                    ip: c.ip.to_string(),
                    authorized: c.authorized,
                    connected_secs: now.saturating_sub(c.connected_ms) / 1000,
                    last_share_secs,
                    accepted_shares: c.accepted_shares,
                    accepted_difficulty: c.accepted_difficulty,
                    difficulty: c.difficulty,
                }
            })
            .collect()
    }
}

pub struct RpcPolicy {
    pub checkpoint: CheckpointStatus,
    pub author: AuthorKeyStatus,
    pub link: crate::validator::AnchorCells,
    pub tip_cell: TipCell,
    pub tx: tokio::sync::mpsc::Sender<Cmd>,
    pub checkpoints_enabled: bool,
}

impl plaine_rpc::views::PolicyView for RpcPolicy {
    fn checkpoint_status(&self) -> CheckpointStatus {
        // recompute the countdown from the live tip every call. a value captured at
        // construction would freeze while the chain moved past it.
        let sunset = plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT;
        let height = self.tip_cell.get().height;
        CheckpointStatus {
            sunset_height: sunset,
            blocks_until_sunset: Some(sunset.saturating_sub(height)),
            sunset_passed: height >= sunset,
            ..self.checkpoint.clone()
        }
    }

    fn checkpoint_link(&self) -> plaine_rpc::views::CheckpointLink {
        let anchor = *self.link.0.lock().expect("anchor");
        let enforced = self.link.1.lock().expect("enforced").len();
        plaine_rpc::views::CheckpointLink::Live {
            last_anchor: anchor.map(|a| (a.height, a.hash)),
            enforced,
        }
    }

    fn checkpoint_submit(
        &self,
        cp: &plaine_consensus::rules::SignedCheckpoint,
    ) -> CheckpointSubmit {
        use plaine_chain::checkpoints::CheckpointOutcome as O;
        if !self.checkpoints_enabled {
            return CheckpointSubmit::NotConfigured;
        }
        let cp = cp.clone();
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        if self
            .tx
            .try_send(Cmd::Checkpoint { cp: Box::new(cp), reply })
            .is_err()
        {
            return CheckpointSubmit::Busy;
        }
        let Ok(v) = rx.recv_timeout(ASK_DEADLINE) else {
            return CheckpointSubmit::Busy;
        };
        match v.report.outcome {
            O::Unverified => CheckpointSubmit::Unverified,
            O::GenesisImmutable => CheckpointSubmit::GenesisImmutable,
            _ if v.report.anchor_advanced => CheckpointSubmit::Advanced {
                height: v.anchor.map(|a| a.height).unwrap_or_default(),
                enforced: v.enforced,
                enforcing: v.report.outcome == O::Admitted,
            },
            _ => CheckpointSubmit::Unchanged,
        }
    }

    fn author_key_status(&self) -> AuthorKeyStatus {
        self.author.clone()
    }
}

pub struct RpcBudgets {
    pub ask: Ask,
    pub cpu_pool_threads: usize,
    pub validator_queue: Arc<std::sync::atomic::AtomicU64>,
    pub shares: Arc<std::sync::atomic::AtomicU64>,
}

impl plaine_rpc::views::BudgetView for RpcBudgets {
    fn budgets(&self) -> Budgets {
        let b = self.ask.ask(Query::Budgets, ASK_DEADLINE).unwrap_or_default();
        Budgets {
            cpu_pool_threads: self.cpu_pool_threads,
            pow_verifies_total: b.pow_calls,

            pow_cache_hit_pct: if b.pow_calls + b.pow_cache_hits == 0 {
                0
            } else {
                (b.pow_cache_hits * 100 / (b.pow_calls + b.pow_cache_hits)) as u32
            },
            stratum_shares_per_sec: self.shares.load(std::sync::atomic::Ordering::Relaxed),
            validator_queue_bytes: self.validator_queue.load(std::sync::atomic::Ordering::Relaxed),
            validator_queue_bytes_cap: crate::wire::net::QUEUE_BYTES,
            validator_queue_items: self.ask.queue_depth(),
            validator_queue_items_cap: self.ask.queue_capacity(),
            chain_events_total: b.headers_connected + b.bodies_validated + b.reorgs,
            mempool_sources: b.sources,
            mempool_sources_cap: b.sources_cap,
            rpc_rejected_busy: self.ask.refused(),
            rss_bytes: resident_set_bytes(),
            body_rejects_already_held: b.body_already_held,
            body_rejects_not_admissible: b.body_not_admissible,
        }
    }
}

fn resident_set_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;

        Some(pages.saturating_mul(4096))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc_chain(v: Option<crate::health::Observation>) -> RpcChain {
        static STORE: std::sync::OnceLock<Arc<NodeStore>> = std::sync::OnceLock::new();
        let store = Arc::clone(STORE.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("plaine-rpcview-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let cfg = plaine_storage::StoreConfig::new(dir, plaine_storage::Network::Main);
            let (_c, reader) = crate::wire::store::tests::open_for_test(cfg);
            Arc::new(NodeStore::new(reader, crate::wire::store::new_ring()))
        }));
        rpc_chain_on(store, v)
    }

    fn rpc_chain_on(store: Arc<NodeStore>, v: Option<crate::health::Observation>) -> RpcChain {
        let (tx, rx) = tokio::sync::mpsc::channel::<Cmd>(1);
        drop(rx);
        RpcChain {
            tip: TipCell::new(crate::wire::tip::TipView {
                height: 487,

                time: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                ..crate::wire::tip::TipView::default()
            }),
            store,
            notes: NoteIndex::default(),
            ask: Ask::new(tx),
            network: plaine_rpc::views::Network::Main,
            pruned: false,
            txindex: false,
            best_known: Arc::new(std::sync::atomic::AtomicU64::new(426)),
            peers: Arc::new(std::sync::atomic::AtomicUsize::new(4)),
            verdict: Arc::new(std::sync::Mutex::new(v.map(|obs| crate::health::Latched {
                obs,
                at: std::time::Instant::now(),
            }))),
        }
    }

    #[test]
    fn info_serves_heartbeat_verdict() {
        use plaine_rpc::views::ChainView;
        let info = rpc_chain(Some(frozen_peer_obs())).info();
        assert_eq!(
            info.sync,
            plaine_rpc::SyncStatus::Stalled,
            "chain_getInfo must serve the heartbeat's stalled verdict, got {:?}",
            info.sync
        );
        assert!(
            info.stall_reason.unwrap_or_default().contains("no peer"),
            "and it must be the frozen-peer arm, not the bare quiet arm"
        );
    }

    fn frozen_peer_obs() -> crate::health::Observation {
        crate::health::Observation {
            started: true,
            height: 487,
            best_known_height: Some(487),
            tip_age_secs: 3,
            idle_secs: 3,
            gap_idle_secs: 3,
            peer_height_idle_secs: 600,
            peers: 4,
            ..Default::default()
        }
    }

    #[test]
    fn info_no_stall_refuted_by_height() {
        use plaine_rpc::views::ChainView;
        let quiet = crate::health::Observation {
            started: true,
            height: 486,
            best_known_height: Some(486),
            tip_age_secs: crate::health::QUIET_STALL_AFTER_SECS,
            idle_secs: crate::health::QUIET_STALL_AFTER_SECS,
            peers: 4,
            ..Default::default()
        };

        assert_eq!(
            crate::health::assess(&quiet).status,
            plaine_rpc::SyncStatus::Stalled,
            "the control: the heartbeat was entitled to this verdict when it computed it"
        );

        let info = rpc_chain(Some(quiet)).info();
        assert!(
            info.tip_age_secs < crate::health::QUIET_STALL_AFTER_SECS,
            "the harness must be serving a fresh tip, or this proves nothing"
        );
        assert_ne!(
            info.sync,
            plaine_rpc::SyncStatus::Stalled,
            "live facts say a block arrived (height {}, tip {}s) but it answered {:?}: {:?}",
            info.height,
            info.tip_age_secs,
            info.sync,
            info.stall_reason
        );
        assert!(
            !info.stall_reason.unwrap_or_default().contains("no new block for"),
            "and above all it must not print the sentence the rest of the reply refutes"
        );
    }

    #[test]
    fn halted_reported_first() {
        use plaine_rpc::views::ChainView;
        let mut c = rpc_chain(Some(crate::health::Observation {
            started: true,
            height: 487,
            best_known_height: Some(487),
            tip_age_secs: 3,
            idle_secs: 3,
            peers: 4,
            ..Default::default()
        }));

        assert_eq!(c.info().sync, plaine_rpc::SyncStatus::Synced);
        c.tip = TipCell::new(crate::wire::tip::TipView {
            height: 487,
            halted: Some("the store refused a write"),
            ..crate::wire::tip::TipView::default()
        });
        let info = c.info();
        assert_eq!(info.sync, plaine_rpc::SyncStatus::Stalled);
        assert!(info.stall_reason.unwrap_or_default().contains("halted"));
    }

    #[test]
    fn info_starting_before_first_beat() {
        use plaine_rpc::views::ChainView;
        let info = rpc_chain(None).info();
        assert_eq!(info.sync, plaine_rpc::SyncStatus::Starting);
        assert_eq!(info.stall_reason, None);
    }

    #[test]
    fn unanswered_checkpoint_is_busy() {
        use plaine_rpc::views::PolicyView;

        let (tx, rx) = tokio::sync::mpsc::channel::<Cmd>(1);
        drop(rx);
        let sunset = plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT;
        let p = RpcPolicy {
            checkpoint: CheckpointStatus {
                enabled: true,
                key_source: plaine_rpc::views::KeySource::Config,
                key_fingerprints: vec!["deadbeef".into()],
                threshold: 1,
                last_anchor: None,
                enforced_count: 0,
                sunset_height: sunset,
                blocks_until_sunset: Some(1),
                sunset_passed: false,
            },
            author: AuthorKeyStatus {
                enabled: true,
                key_source: plaine_rpc::views::KeySource::Embedded,
                fingerprint: "0badf00d".into(),
                show_in_log: false,
            },
            tip_cell: TipCell::default(),
            link: (
                Arc::new(std::sync::Mutex::new(None)),
                Arc::new(std::sync::Mutex::new(Arc::new(Vec::new()))),
                Arc::new(std::sync::Mutex::new(None)),
            ),
            tx,
            checkpoints_enabled: true,
        };
        let cp = plaine_consensus::rules::SignedCheckpoint {
            height: 900,
            hash: [7u8; 32],
            sigs: Vec::new(),
        };
        assert_eq!(
            p.checkpoint_submit(&cp),
            CheckpointSubmit::Busy,
            "a record that never reached the chain must be retryable"
        );

        let off = RpcPolicy { checkpoints_enabled: false, ..p };
        assert_eq!(off.checkpoint_submit(&cp), CheckpointSubmit::NotConfigured);
    }

    fn note_body(payloads: &[&[u8]]) -> Vec<u8> {
        use plaine_consensus::codec::{AnnouncementTx, BlockBody, AuthorNote as _AuthorNote};
        let _ = _AuthorNote { encoding: 0, payload: Vec::new() };
        let txs: Vec<Vec<u8>> = payloads
            .iter()
            .enumerate()
            .map(|(i, p)| {
                AnnouncementTx {
                    from_pub: [1u8; 32],
                    fee: 1,
                    nonce: i as u64,
                    encoding: 0,
                    payload: p.to_vec(),
                    sig: [0u8; 64],
                }
                .encode()
                .expect("encodes")
            })
            .collect();
        let refs: Vec<&[u8]> = txs.iter().map(|t| t.as_slice()).collect();
        BlockBody::encode(&refs).expect("body encodes")
    }

    #[test]
    fn refusals_keep_distinct_tags() {
        let broke = map_reject(&Reject::InsufficientBalance { index: 0, need: 1_000_000, have: 0 });
        let forged = map_reject(&Reject::Tx { index: 0, err: TxError::NotAuthorKey });
        assert_eq!(broke.tag(), "insufficient-funds");
        assert_eq!(forged.tag(), "not-author-key");
        assert_ne!(broke.tag(), forged.tag());

        assert!(
            broke.human().contains("1000000"),
            "the refusal must carry the real figures: {}",
            broke.human()
        );

        let h = forged.human();
        assert!(h.contains("author"), "{h}");
        assert!(!h.contains("mile"), "a wrong-key refusal must not quote any balance: {h}");
    }

    #[test]
    fn rejects_map_to_tags_no_debug_leak() {
        for r in [
            Reject::TxDecode,
            Reject::TxTooLarge { got: 9_000 },
            Reject::BelowRelayFloor { fee: 1, floor: 1_000_000 },
            Reject::BadTransferSignature { index: 0 },
            Reject::TxStale { next: 4, got: 2 },
            Reject::TxKnown,
            Reject::PoolFull,
            Reject::Busy,
            Reject::Tx { index: 0, err: TxError::NotAuthorKey },
            Reject::Tx { index: 0, err: TxError::AnnouncementLength { len: 0 } },
        ] {
            let e = map_reject(&r);
            let dbg = format!("{r:?}");
            assert!(!e.human().contains(&dbg), "{} leaks its debug text", e.tag());
            assert!(!e.tag().is_empty());
        }
    }

    #[test]
    fn two_notes_both_paged() {
        let ix = NoteIndex::new();
        ix.on_block(5, [0u8; 32], &note_body(&[b"first", b"second"]));
        assert_eq!(ix.page(NotesCursor::Newest, 10, 10).total, 2);
        let p1 = ix.page(NotesCursor::Newest, 1, 10);
        assert_eq!(p1.notes.len(), 1);
        assert_eq!(p1.notes[0].payload, b"second");
        assert!(p1.more);
        let next = p1.next_seq.expect("more implies a cursor");
        let p2 = ix.page(NotesCursor::Before(next), 10, 10);
        assert_eq!(p2.notes.len(), 1);
        assert_eq!(p2.notes[0].payload, b"first");
        assert!(!p2.more);
        assert_eq!(p2.total, 2, "total is the true count regardless of the cursor");
    }

    #[test]
    fn confirmations_never_zero() {
        let ix = NoteIndex::new();
        ix.on_block(5, [0u8; 32], &note_body(&[b"x"]));
        let p = ix.page(NotesCursor::Newest, 10, 5);
        assert_eq!(p.notes[0].confirmations, 1, "a note in the tip block has one confirmation");
    }

    #[test]
    fn reorg_removes_orphaned_notes() {
        let ix = NoteIndex::new();
        ix.on_block(5, [0u8; 32], &note_body(&[b"kept"]));
        ix.on_block(6, [1u8; 32], &note_body(&[b"orphaned"]));
        assert_eq!(ix.page(NotesCursor::Newest, 10, 10).total, 2);
        ix.rollback_above(5);
        assert_eq!(ix.page(NotesCursor::Newest, 10, 10).total, 1);
        let p = ix.page(NotesCursor::Newest, 10, 5);
        assert_eq!(p.notes[0].payload, b"kept");
        assert_eq!(p.notes[0].seq, 0, "sequence numbers are compacted after a reorg");
    }

    #[test]
    fn fee_percentiles_are_nearest_rank_and_never_below_the_floor() {
        let s = suggest_from(240, &mut [], 7);
        assert_eq!((s.p10_mile, s.p50_mile, s.p90_mile), (7, 7, 7), "no transfers: the floor");
        assert_eq!(s.blocks_sampled, 240);

        let mut fees: Vec<u128> = (1..=100).rev().collect();
        let s = suggest_from(3, &mut fees, 1);
        assert_eq!((s.p10_mile, s.p50_mile, s.p90_mile), (10, 50, 90));

        let s = suggest_from(3, &mut [2, 40, 3], 5);
        assert_eq!((s.p10_mile, s.p50_mile, s.p90_mile), (5, 5, 40), "clamped to the floor");
        assert_eq!(s.relay_floor_mile, 5);

        let s = suggest_from(1, &mut [9], 1);
        assert_eq!((s.p10_mile, s.p50_mile, s.p90_mile), (9, 9, 9), "one fee is every percentile");
    }

    fn body_with_fees(height: u64, fees: &[u128]) -> Vec<u8> {
        use plaine_consensus::codec::{AuthorNote, BlockBody, CoinbaseTx, TransferTx};
        let cb = CoinbaseTx {
            height,
            to: [0x77; 20],
            reward: plaine_consensus::emission::block_reward(height),
            fees: fees.iter().sum(),
            note: AuthorNote { encoding: 0x01, payload: Vec::new() },
        };
        let mut recs = vec![cb.encode().expect("coinbase")];
        for (n, &fee) in fees.iter().enumerate() {
            let t = TransferTx {
                from_pub: [n as u8 + 1; 32],
                to: [0x22; 20],
                amount: 1_000,
                fee,
                nonce: 0,
                sig: [0u8; 64],
            };
            recs.push(t.encode().to_vec());
        }
        let refs: Vec<&[u8]> = recs.iter().map(|r| r.as_slice()).collect();
        BlockBody::encode(&refs).expect("body")
    }

    #[test]
    fn fee_sampling_reads_transfers_back_from_the_tip_and_stops_at_a_gap() {
        // Heights 10 and 12..=14 are held; 11 is missing, as below a pruning horizon.
        let blocks = vec![
            (10, [10u8; 32], body_with_fees(10, &[1_000_000])),
            (12, [12u8; 32], body_with_fees(12, &[5, 50])),
            (13, [13u8; 32], body_with_fees(13, &[])),
            (14, [14u8; 32], body_with_fees(14, &[7])),
        ];
        let (dir, _c, store) = crate::wire::store::tests::store_with_ring(blocks);

        let (n, mut fees) = sample_transfer_fees(&store, 14, 240);
        assert_eq!(n, 3, "14, 13, 12 are read; the gap at 11 ends the sample");
        fees.sort_unstable();
        assert_eq!(fees, [5, 7, 50], "only transfers count; block 10 is past the gap");

        let (n, fees) = sample_transfer_fees(&store, 14, 2);
        assert_eq!((n, fees.len()), (2, 1), "the window is `count` blocks: 14 and 13");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_side_branch_hash_is_not_served_the_best_chain_body() {
        use plaine_rpc::views::{ChainView, Verbosity};
        let (best, side) = ([0x05; 32], [0xAB; 32]);
        let (dir, _committer, store) = crate::wire::store::tests::store_with_fork(5, best, side);
        let chain = rpc_chain_on(Arc::new(store), None);

        let b = chain.block_by_hash(&best, Verbosity::HeaderAndTxids).expect("the best block");
        assert_eq!(b.author_note, b"best");
        assert!(chain.header_by_hash(&side).is_some(), "the side header itself is known");
        assert!(
            chain.block_by_hash(&side, Verbosity::HeaderAndTxids).is_none(),
            "the body stored at height 5 belongs to another block"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_reorg_keeps_the_notes_of_the_branch_it_applies() {
        let ix = NoteIndex::new();
        ix.on_block(5, [0u8; 32], &note_body(&[b"kept"]));
        ix.on_block(6, [1u8; 32], &note_body(&[b"orphaned"]));
        let replacement = note_body(&[b"replacement"]);
        let longer = note_body(&[b"on top"]);
        ix.on_reorg(5, [(6, [2u8; 32], &replacement[..]), (7, [3u8; 32], &longer[..])]);
        let p = ix.page(NotesCursor::Newest, 10, 7);
        let payloads: Vec<&[u8]> = p.notes.iter().map(|n| n.payload.as_slice()).collect();
        assert_eq!(payloads, [&b"on top"[..], b"replacement", b"kept"], "newest first");
        assert_eq!(p.total, 3);
    }

    #[test]
    fn txid_hint_verified_against_body() {
        use plaine_consensus::codec::{AuthorNote, BlockBody, CoinbaseTx, Tx};
        let cb = |h: u64| CoinbaseTx {
            height: h,
            to: [0x77; 20],
            reward: plaine_consensus::emission::block_reward(h),
            fees: 0,
            note: AuthorNote { encoding: 0x01, payload: vec![0x41; 4] },
        };
        let recs: Vec<Vec<u8>> = (10..14).map(|h| cb(h).encode().expect("encode")).collect();
        let refs: Vec<&[u8]> = recs.iter().map(|v| v.as_slice()).collect();
        let raw = BlockBody::encode(&refs).expect("body");
        let body = BlockBody::parse(&raw).expect("parse");
        let id_of = |i: usize| match body.decode_tx(i).unwrap().unwrap() {
            Tx::Coinbase(c) => c.txid().unwrap(),
            _ => unreachable!(),
        };
        let want = id_of(2);

        assert_eq!(verify_txid_hint(&body, &want, 2), Some(2));

        assert_eq!(verify_txid_hint(&body, &want, 0), Some(2));
        assert_eq!(verify_txid_hint(&body, &want, 3), Some(2));

        assert_eq!(verify_txid_hint(&body, &want, 9), Some(2));

        let absent = [0xEE; 32];
        assert_eq!(verify_txid_hint(&body, &absent, 1), None);
    }

    #[test]
    fn sunset_countdown_follows_tip() {
        use plaine_rpc::views::PolicyView;
        let sunset = plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT;
        let tip = TipCell::default();
        let p = policy_with_tip(tip.clone());

        let s = p.checkpoint_status();
        assert_eq!(s.blocks_until_sunset, Some(sunset), "at height 0 the whole sunset is ahead");
        assert!(!s.sunset_passed);

        publish_height(&tip, 1_000);
        let s = p.checkpoint_status();
        assert_eq!(
            s.blocks_until_sunset,
            Some(sunset - 1_000),
            "the countdown must follow the tip, not a value captured at construction"
        );
        assert!(!s.sunset_passed);

        publish_height(&tip, sunset + 7);
        let s = p.checkpoint_status();
        assert_eq!(s.blocks_until_sunset, Some(0), "saturating, never negative");
        assert!(
            s.sunset_passed,
            "the chain is past {sunset} and the node still reports the lever live"
        );

        assert_eq!(s.sunset_height, sunset);
    }

    #[cfg(test)]
    fn publish_height(cell: &TipCell, height: u64) {
        cell.publish(crate::wire::tip::TipView { height, ..Default::default() });
    }

    #[cfg(test)]
    fn policy_with_tip(tip_cell: TipCell) -> RpcPolicy {
        let (tx, rx) = tokio::sync::mpsc::channel::<Cmd>(1);

        std::mem::forget(rx);
        RpcPolicy {
            checkpoint: CheckpointStatus {
                enabled: true,
                key_source: plaine_rpc::views::KeySource::Config,
                key_fingerprints: vec!["deadbeef".into()],
                threshold: 1,
                last_anchor: None,
                enforced_count: 0,
                sunset_height: 1,
                blocks_until_sunset: Some(999_999),
                sunset_passed: true,
            },
            author: AuthorKeyStatus {
                enabled: true,
                key_source: plaine_rpc::views::KeySource::Embedded,
                fingerprint: "0badf00d".into(),
                show_in_log: false,
            },
            tip_cell,
            link: (
                Arc::new(std::sync::Mutex::new(None)),
                Arc::new(std::sync::Mutex::new(Arc::new(Vec::new()))),
                Arc::new(std::sync::Mutex::new(None)),
            ),
            tx,
            checkpoints_enabled: true,
        }
    }

}

#[cfg(test)]
mod history_describe {
    use super::*;
    use plaine_consensus::codec::{AnnouncementTx, AuthorNote, CoinbaseTx, Tx, TransferTx};
    use plaine_consensus::crypto::address_payload;

    const A: [u8; 32] = [1; 32];
    const B: [u8; 32] = [2; 32];

    fn cb(to: [u8; 20]) -> Tx {
        Tx::Coinbase(CoinbaseTx {
            height: 5,
            to,
            reward: 200_000,
            fees: 3_000,
            note: AuthorNote { encoding: 0, payload: Vec::new() },
        })
    }

    fn xfer(from: [u8; 32], to: [u8; 20]) -> Tx {
        Tx::Transfer(TransferTx { from_pub: from, to, amount: 7_000, fee: 1_000, nonce: 0, sig: [0; 64] })
    }

    #[test]
    fn a_coinbase_credits_reward_plus_fees_to_its_recipient_only() {
        let a = address_payload(&A);
        let got = describe(&cb(a), &a).expect("A is the recipient");
        assert_eq!(got, (HistoryKind::Coinbase, Direction::In, 203_000, 0, None));
        assert_eq!(describe(&cb(a), &address_payload(&B)), None);
    }

    #[test]
    fn a_transfer_reads_from_each_side() {
        let (a, b) = (address_payload(&A), address_payload(&B));
        assert_eq!(
            describe(&xfer(A, b), &a),
            Some((HistoryKind::Transfer, Direction::Out, 7_000, 1_000, Some(b)))
        );
        assert_eq!(
            describe(&xfer(A, b), &b),
            Some((HistoryKind::Transfer, Direction::In, 7_000, 1_000, Some(a)))
        );
        assert_eq!(
            describe(&xfer(A, a), &a),
            Some((HistoryKind::Transfer, Direction::SelfTransfer, 7_000, 1_000, Some(a)))
        );
    }

    // This is what drops rows a reorg left behind: the index still points at
    // (height, index), but the canonical block there now holds a transaction
    // between other parties.
    #[test]
    fn a_transaction_between_others_is_not_part_of_the_history() {
        let c = address_payload(&[3; 32]);
        assert_eq!(describe(&xfer(A, address_payload(&B)), &c), None);
    }

    #[test]
    fn an_announcement_is_an_outgoing_fee_for_its_author() {
        let ann = Tx::Announcement(AnnouncementTx {
            from_pub: A,
            fee: 5_000,
            nonce: 1,
            encoding: 0,
            payload: b"hello".to_vec(),
            sig: [0; 64],
        });
        assert_eq!(
            describe(&ann, &address_payload(&A)),
            Some((HistoryKind::Announcement, Direction::Out, 0, 5_000, None))
        );
        assert_eq!(describe(&ann, &address_payload(&B)), None);
    }
}
