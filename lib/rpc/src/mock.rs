use std::sync::Arc;

use crate::json::Json;
use crate::views::{
    AccountRecord, Address20, AuthorKeyStatus, AuthorNote, AuthorNotesPage, BlockRecord, BudgetView,
    Budgets, ChainInfo, ChainView, CheckpointLink, CheckpointStatus, CheckpointSubmit,
    EmissionAudit, FeeSuggestion,
    Hash32, HeaderRecord, HistoryEntry, HistoryLookup, KeySource, MempoolInfo, MempoolView, NetView, Network, Node, NotesCursor,
    PeerInfo, PolicyView, StratumSession, StratumView, SubmitError, SyncStatus, TxLookup, TxRecord,
    Verbosity,
};

#[derive(Clone)]
pub struct MockNode {
    network: Network,
    height: u64,
    sync: SyncStatus,
    stall_reason: Option<String>,
    peers: Vec<PeerInfo>,
    sessions: Vec<StratumSession>,
    notes: Vec<AuthorNote>,
    txindex: bool,
    pruned: bool,
    prune_horizon: u64,
    checkpoints_enabled: bool,
    author_enabled: bool,
    checkpoint_status: Option<CheckpointStatus>,
    checkpoint_link: CheckpointLink,
    checkpoint_submit: CheckpointSubmit,
    author_status: Option<AuthorKeyStatus>,
    // None: the node runs without addrindex.
    history: Option<(u64, Vec<HistoryEntry>)>,
}

impl MockNode {
    pub fn synced() -> MockNode {
        MockNode {
            network: Network::Main,
            height: 12_345,
            sync: SyncStatus::Synced,
            stall_reason: None,
            peers: (0..8).map(sample_peer).collect(),
            sessions: Vec::new(),
            notes: Vec::new(),
            txindex: false,
            pruned: false,
            prune_horizon: 0,
            checkpoints_enabled: true,
            author_enabled: true,
            checkpoint_status: None,
            checkpoint_link: CheckpointLink::Live {
                last_anchor: Some((12_300, [7u8; 32])),
                enforced: 3,
            },
            checkpoint_submit: CheckpointSubmit::Advanced {
                height: 12_340,
                enforced: 4,
                enforcing: true,
            },
            author_status: None,
            history: None,
        }
    }

    pub fn with_key_status(
        mut self,
        checkpoint: CheckpointStatus,
        author: AuthorKeyStatus,
    ) -> MockNode {
        self.checkpoint_status = Some(checkpoint);
        self.author_status = Some(author);
        self
    }

    pub fn stalled() -> MockNode {
        MockNode {
            sync: SyncStatus::Stalled,
            stall_reason: Some(
                "no peers connected and no new block for 2h4m; check your network".into(),
            ),
            peers: Vec::new(),
            ..MockNode::synced()
        }
    }

    pub fn with_session(mut self, s: StratumSession) -> MockNode {
        self.sessions.push(s);
        self
    }

    pub fn with_network(mut self, network: Network) -> MockNode {
        self.network = network;
        self
    }

    pub fn with_notes(mut self) -> MockNode {
        for (i, text) in [
            "Plaine mainnet is live. Never type a key into a website.".to_string(),
            format!(
                "Reminder: do not accept large payments at fewer than {} confirmations while the \
                 chain is young.",
                plaine_consensus::constants::MAX_REORG_DEPTH
            ),
            "ASIC sighting confirmed. Isochron v2 activates at height 600000 - update your miner \
             before then."
                .to_string(),
        ]
        .iter()
        .enumerate()
        {
            let height = 1000 + (i as u64) * 100;
            self.notes.push(AuthorNote {
                seq: self.notes.len() as u64,
                height,
                txid: [i as u8 + 1; 32],
                time: 1_760_000_000 + height * 60,
                confirmations: self.height - height + 1,
                encoding: 0x01,
                payload: text.as_bytes().to_vec(),
            });
        }
        self
    }

    pub fn with_notes_sharing_a_height(mut self) -> MockNode {
        for (i, text) in ["first in the block", "second in the block", "third in the block"]
            .iter()
            .enumerate()
        {
            self.notes.push(AuthorNote {
                seq: self.notes.len() as u64,
                height: 500,
                txid: [0x50 + i as u8; 32],
                time: 1_760_000_000,
                confirmations: self.height - 500 + 1,
                encoding: 0x01,
                payload: text.as_bytes().to_vec(),
            });
        }
        self
    }

    pub fn with_hostile_note(mut self) -> MockNode {
        self.notes.push(AuthorNote {
            seq: self.notes.len() as u64,
            height: 900,
            txid: [9u8; 32],
            time: 1_759_000_000,
            confirmations: 1,
            encoding: 0x01,
            payload: b"\x1b[2J\x1b[31mSEND COINS TO\rplne1fake".to_vec(),
        });
        self
    }

    pub fn with_hostile_peer(mut self) -> MockNode {
        self.peers = vec![PeerInfo {
            id: 1,
            addr: "203.0.113.7:9256".into(),
            outbound: false,
            connected_secs: 30,
            best_height: 12_345,
            bytes_recv: 1024,
            bytes_sent: 2048,
            misbehaviour: 0,
            user_agent: "plaine/9.9\u{1b}[31m\nFAKE LOG LINE".into(),
        }];
        self
    }

    pub fn with_checkpoints_disabled(mut self) -> MockNode {
        self.checkpoints_enabled = false;
        self
    }

    pub fn with_checkpoint_ingest_severed(mut self) -> MockNode {
        self.checkpoint_link = CheckpointLink::Severed;
        self.checkpoint_submit = CheckpointSubmit::Severed;
        self
    }

    pub fn with_checkpoint_submit(mut self, out: CheckpointSubmit) -> MockNode {
        self.checkpoint_submit = out;
        self
    }

    /// Runs with addrindex from `indexed_from`, holding `entries` in any order.
    pub fn with_history(mut self, indexed_from: u64, entries: Vec<HistoryEntry>) -> MockNode {
        self.history = Some((indexed_from, entries));
        self
    }

    pub fn with_txindex(mut self) -> MockNode {
        self.txindex = true;
        self
    }

    pub fn pruned_at(mut self, horizon: u64) -> MockNode {
        self.pruned = true;
        self.prune_horizon = horizon;
        self
    }

    pub fn sample_address() -> String {
        plaine_consensus::bech32m::encode_bytes(
            plaine_consensus::constants::ADDRESS_HRP,
            &[0x11u8; 20],
        )
        .expect("20 bytes always encode")
    }

    pub fn into_node(self) -> Node {
        let me = Arc::new(self);
        Node {
            chain: me.clone(),
            mempool: me.clone(),
            net: me.clone(),
            stratum: me.clone(),
            policy: me.clone(),
            budgets: me,
        }
    }
}

fn sample_peer(i: u64) -> PeerInfo {
    PeerInfo {
        id: i,
        addr: format!("198.51.100.{}:9256", 10 + i),
        outbound: i % 2 == 0,
        connected_secs: 300 + i * 17,
        best_height: 12_345,
        bytes_recv: 100_000 + i * 1_000,
        bytes_sent: 50_000 + i * 700,
        misbehaviour: 0,
        user_agent: "plaine-noded/0.1.0".into(),
    }
}

fn sample_header(height: u64, tip: u64) -> HeaderRecord {
    let h = plaine_consensus::codec::Header {
        version: plaine_consensus::constants::VERSION_BASE,
        height,
        prev_hash: [0u8; 32],
        tx_root: [0u8; 32],
        ext_root: [0u8; 32],
        time: 1_760_000_000 + height * 60,
        bits: 0x1f00_ffff,
        author_note_len: 0,
        nonce: height,
    };
    let raw = h.encode();
    let hash = h.hash();
    let mut chainwork = [0u8; 32];
    chainwork[24..].copy_from_slice(&height.to_be_bytes());
    HeaderRecord {
        raw,
        hash,
        height,
        confirmations: tip.saturating_sub(height) + 1,
        canonical: true,
        chainwork,
    }
}

fn sample_block(header: HeaderRecord, verbosity: Verbosity) -> BlockRecord {
    let txids: Vec<Hash32> = (0..3u8).map(|i| [i + 40; 32]).collect();
    let txs = if matches!(verbosity, Verbosity::FullTxs) {
        txids.iter().map(|_| Json::Obj(Vec::new())).collect()
    } else {
        Vec::new()
    };
    BlockRecord {
        tx_count: txids.len(),
        size_bytes: 512,
        author_note: Vec::new(),
        raw: matches!(verbosity, Verbosity::RawHex).then(|| header.raw.to_vec()),
        txs,
        txids,
        header,
    }
}

impl ChainView for MockNode {
    fn info(&self) -> ChainInfo {
        let tip = sample_header(self.height, self.height);
        ChainInfo {
            network: self.network,
            version: concat!("plaine-noded/", env!("CARGO_PKG_VERSION")).to_string(),
            height: self.height,
            tip_hash: tip.hash,
            chainwork: tip.chainwork,
            tip_time: 1_760_000_000 + self.height * 60,
            tip_age_secs: match self.sync {
                SyncStatus::Synced => 12,
                SyncStatus::Stalled => 7_440,
                _ => 3_600,
            },
            sync: self.sync,
            best_known_height: (!self.peers.is_empty()).then_some(self.height),
            stall_reason: self.stall_reason.clone(),
            txindex: self.txindex,
            pruned: self.pruned,
            prune_horizon_height: self.prune_horizon,
        }
    }

    fn header_by_height(&self, height: u64) -> Option<HeaderRecord> {
        (height <= self.height).then(|| sample_header(height, self.height))
    }

    fn header_by_hash(&self, hash: &Hash32) -> Option<HeaderRecord> {
        // mock: just scan the first handful of heights and match by hash
        (0..=self.height.min(64))
            .map(|h| sample_header(h, self.height))
            .find(|h| h.hash == *hash)
    }

    fn block_by_height(&self, height: u64, verbosity: Verbosity) -> Option<BlockRecord> {
        if height > self.height || (self.pruned && height < self.prune_horizon) {
            return None;
        }
        Some(sample_block(sample_header(height, self.height), verbosity))
    }

    fn block_by_hash(&self, hash: &Hash32, verbosity: Verbosity) -> Option<BlockRecord> {
        let h = self.header_by_hash(hash)?;
        self.block_by_height(h.height, verbosity)
    }

    fn account(&self, _addr: &Address20) -> AccountRecord {
        AccountRecord {
            balance: 4_200_000_000_000_000_000,
            nonce: 7,
            pending_nonce: 8,
            immature: 137_672_000_000_000_000,
        }
    }

    fn tx(&self, _txid: &Hash32) -> TxLookup {
        if self.txindex {
            TxLookup::Absent
        } else {
            TxLookup::NotIndexed { indexed_from: None }
        }
    }

    fn emission_audit(&self, height: u64) -> Option<EmissionAudit> {
        if height > self.height {
            return None;
        }
        let issued = plaine_consensus::emission::cumulative_issued(height);
        Some(EmissionAudit {
            height,
            issued_mile: issued,
            expected_by_formula_mile: issued,
            max_supply_mile: None,
            subsidy_at_height_mile: plaine_consensus::emission::block_reward(height),
        })
    }

    fn account_history(
        &self,
        addr: &Address20,
        before: Option<(u64, u16)>,
        limit: usize,
    ) -> HistoryLookup {
        let Some((indexed_from, all)) = &self.history else {
            return HistoryLookup::NotIndexed;
        };
        let _ = addr;
        let mut sorted: Vec<&HistoryEntry> = all
            .iter()
            .filter(|e| before.is_none_or(|b| (e.height, e.index) < b))
            .collect();
        sorted.sort_by_key(|e| core::cmp::Reverse((e.height, e.index)));
        let more = sorted.len() > limit;
        let entries: Vec<HistoryEntry> = sorted.into_iter().take(limit).cloned().collect();
        let next_cursor = if more { entries.last().map(|e| (e.height, e.index)) } else { None };
        HistoryLookup::Page { indexed_from: *indexed_from, entries, next_cursor, unavailable_below: None }
    }

    fn author_notes(&self, cursor: NotesCursor, limit: usize) -> AuthorNotesPage {
        let mut sorted = self.notes.clone();
        sorted.sort_by_key(|n| core::cmp::Reverse(n.seq));
        let filtered: Vec<AuthorNote> = sorted
            .into_iter()
            .filter(|n| match cursor {
                NotesCursor::Newest => true,
                NotesCursor::AtOrBelowHeight(h) => n.height <= h,
                NotesCursor::Before(s) => n.seq < s,
            })
            .collect();
        let more = filtered.len() > limit;
        let notes: Vec<AuthorNote> = filtered.into_iter().take(limit).collect();

        let next_seq = if more { notes.last().map(|n| n.seq) } else { None };
        AuthorNotesPage { notes, more, total: self.notes.len() as u64, next_seq }
    }
}

impl MempoolView for MockNode {
    fn info(&self) -> MempoolInfo {
        MempoolInfo {
            tx_count: 43,
            bytes: 43 * plaine_consensus::constants::TX_TRANSFER_BYTES,
            executable: 40,
            queued: 3,
            relay_fee_mile: 1_000_000,
            max_txs: plaine_consensus::constants::MAX_MEMPOOL_TXS,
        }
    }

    fn by_sender(&self, _addr: &Address20) -> Vec<TxRecord> {
        Vec::new()
    }

    fn fee_suggest(&self) -> FeeSuggestion {
        FeeSuggestion {
            blocks_sampled: 240,
            p10_mile: 1_000_000,
            p50_mile: 2_000_000,
            p90_mile: 10_000_000,
            relay_floor_mile: 1_000_000,
        }
    }

    fn submit(&self, raw: &[u8]) -> Result<Hash32, SubmitError> {
        if raw.len() < plaine_consensus::constants::TX_TRANSFER_BYTES {
            return Err(SubmitError::Malformed(format!(
                "{} bytes is shorter than the smallest transaction",
                raw.len()
            )));
        }
        Ok(plaine_consensus::blake3::hash(raw))
    }
}

impl NetView for MockNode {
    fn peers(&self) -> Vec<PeerInfo> {
        self.peers.clone()
    }
}

impl StratumView for MockNode {
    fn sessions(&self) -> Vec<StratumSession> {
        self.sessions.clone()
    }
}

impl PolicyView for MockNode {
    fn checkpoint_status(&self) -> CheckpointStatus {
        if let Some(s) = &self.checkpoint_status {
            return s.clone();
        }
        let sunset = plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT;
        CheckpointStatus {
            enabled: self.checkpoints_enabled,
            key_source: KeySource::Embedded,
            key_fingerprints: vec!["9f2c8a41".into()],
            threshold: 1,
            last_anchor: Some((12_300, [7u8; 32])),
            enforced_count: 3,
            sunset_height: sunset,
            blocks_until_sunset: sunset.checked_sub(self.height),
            sunset_passed: self.height >= sunset,
        }
    }

    fn checkpoint_link(&self) -> CheckpointLink {
        self.checkpoint_link.clone()
    }

    fn checkpoint_submit(
        &self,
        _cp: &plaine_consensus::rules::SignedCheckpoint,
    ) -> CheckpointSubmit {
        self.checkpoint_submit
    }

    fn author_key_status(&self) -> AuthorKeyStatus {
        if let Some(s) = &self.author_status {
            return s.clone();
        }
        AuthorKeyStatus {
            enabled: self.author_enabled,
            key_source: KeySource::Embedded,
            fingerprint: "4d1e77b0".into(),
            show_in_log: true,
        }
    }
}

impl BudgetView for MockNode {
    fn budgets(&self) -> Budgets {
        Budgets {
            cpu_pool_threads: 4,
            pow_verifies_total: 0,
            pow_cache_hit_pct: 0,
            stratum_shares_per_sec: 0,
            validator_queue_bytes: 0,
            validator_queue_bytes_cap: 32 * 1024 * 1024,
            validator_queue_items: 0,
            validator_queue_items_cap: 1024,
            chain_events_total: 0,
            mempool_sources: 1,
            mempool_sources_cap: 128,
            rpc_rejected_busy: 0,
            rss_bytes: Some(96 * 1024 * 1024),
            body_rejects_already_held: 2_104,
            body_rejects_not_admissible: 0,
        }
    }
}
