use crate::json::Json;

pub type Hash32 = [u8; 32];

pub type Address20 = [u8; 20];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Main,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        "main"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Starting,
    Syncing,
    Synced,
    Stalled,
}

impl SyncStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncStatus::Starting => "starting",
            SyncStatus::Syncing => "syncing",
            SyncStatus::Synced => "synced",
            SyncStatus::Stalled => "stalled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChainInfo {
    pub network: Network,
    pub version: String,
    pub height: u64,
    pub tip_hash: Hash32,
    pub chainwork: [u8; 32],
    pub tip_time: u64,
    pub tip_age_secs: u64,
    pub sync: SyncStatus,
    pub best_known_height: Option<u64>,
    pub stall_reason: Option<String>,
    pub txindex: bool,
    pub pruned: bool,
    pub prune_horizon_height: u64,
}

#[derive(Clone, Debug)]
pub struct HeaderRecord {
    pub raw: [u8; 132],
    pub hash: Hash32,
    pub height: u64,
    pub confirmations: u64,
    pub canonical: bool,
    pub chainwork: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    RawHex,
    HeaderAndTxids,
    FullTxs,
}

impl Verbosity {
    pub fn from_int(v: u64) -> Option<Verbosity> {
        match v {
            0 => Some(Verbosity::RawHex),
            1 => Some(Verbosity::HeaderAndTxids),
            2 => Some(Verbosity::FullTxs),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlockRecord {
    pub header: HeaderRecord,
    pub tx_count: usize,
    pub size_bytes: usize,
    pub author_note: Vec<u8>,
    pub raw: Option<Vec<u8>>,
    pub txids: Vec<Hash32>,
    pub txs: Vec<Json>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AccountRecord {
    pub balance: u128,
    pub nonce: u64,
    pub pending_nonce: u64,
    pub immature: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxLocation {
    Mempool,
    Block {
        height: u64,
        confirmations: u64,
    },
}

#[derive(Clone, Debug)]
pub enum TxLookup {
    Found(TxRecord),
    Absent,
    NotIndexed {
        indexed_from: Option<u64>,
    },
}

#[derive(Clone, Debug)]
pub struct TxRecord {
    pub txid: Hash32,
    pub type_byte: u8,
    pub raw: Vec<u8>,
    pub location: TxLocation,
    pub decoded: Json,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MempoolInfo {
    pub tx_count: usize,
    pub bytes: usize,
    pub executable: usize,
    pub queued: usize,
    pub relay_fee_mile: u128,
    pub max_txs: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeeSuggestion {
    pub blocks_sampled: u64,
    pub p10_mile: u128,
    pub p50_mile: u128,
    pub p90_mile: u128,
    pub relay_floor_mile: u128,
}

#[derive(Clone, Copy, Debug)]
pub struct EmissionAudit {
    pub height: u64,
    pub issued_mile: u128,
    pub expected_by_formula_mile: u128,
    pub max_supply_mile: Option<u128>,
    pub subsidy_at_height_mile: u128,
}

#[derive(Clone, Debug)]
pub struct AuthorNote {
    pub seq: u64,
    pub height: u64,
    pub txid: Hash32,
    pub time: u64,
    pub confirmations: u64,
    pub encoding: u8,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NotesCursor {
    #[default]
    Newest,
    AtOrBelowHeight(u64),
    Before(u64),
}

#[derive(Clone, Debug, Default)]
pub struct AuthorNotesPage {
    pub notes: Vec<AuthorNote>,
    pub more: bool,
    pub total: u64,
    pub next_seq: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub id: u64,
    pub addr: String,
    pub outbound: bool,
    pub connected_secs: u64,
    pub best_height: u64,
    pub bytes_recv: u64,
    pub bytes_sent: u64,
    pub misbehaviour: u32,
    pub user_agent: String,
}

// One live stratum session, already reduced to display form. Times are ages in
// seconds; last_share_secs is -1 when the miner has not landed a share yet.
#[derive(Clone, Debug)]
pub struct StratumSession {
    pub worker: String,
    pub address: String,
    pub rig: String,
    pub ip: String,
    pub authorized: bool,
    pub connected_secs: u64,
    pub last_share_secs: i64,
    pub accepted_shares: u64,
    pub accepted_difficulty: u128,
    pub difficulty: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    Embedded,
    Config,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::Embedded => "embedded",
            KeySource::Config => "config",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CheckpointStatus {
    pub enabled: bool,
    pub key_source: KeySource,
    pub key_fingerprints: Vec<String>,
    pub threshold: usize,
    pub last_anchor: Option<(u64, Hash32)>,
    pub enforced_count: usize,
    pub sunset_height: u64,
    pub blocks_until_sunset: Option<u64>,
    pub sunset_passed: bool,
}

// whether a signed checkpoint can reach the chain on this build. Severed is not a config error -
// the embedder never wired the ingest, and no config fixes that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointLink {
    Live {
        last_anchor: Option<(u64, Hash32)>,
        enforced: usize,
    },
    Severed,
}

impl CheckpointLink {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckpointLink::Live { .. } => "live",
            CheckpointLink::Severed => "severed",
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, CheckpointLink::Live { .. })
    }
}

#[derive(Clone, Debug)]
pub struct AuthorKeyStatus {
    pub enabled: bool,
    pub key_source: KeySource,
    pub fingerprint: String,
    pub show_in_log: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Budgets {
    pub cpu_pool_threads: usize,
    pub pow_verifies_total: u64,
    pub pow_cache_hit_pct: u32,
    pub stratum_shares_per_sec: u64,
    pub validator_queue_bytes: u64,
    pub validator_queue_bytes_cap: u64,
    pub validator_queue_items: usize,
    pub validator_queue_items_cap: usize,
    pub chain_events_total: u64,
    pub mempool_sources: usize,
    pub mempool_sources_cap: usize,
    pub rpc_rejected_busy: u64,
    pub rss_bytes: Option<u64>,
    pub body_rejects_already_held: u64,
    pub body_rejects_not_admissible: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Malformed(String),
    TooLarge {
        len: usize,
        max: usize,
    },
    FeeBelowRelayFloor {
        fee: u128,
        floor: u128,
    },
    BadSignature,
    NonceOutOfRange {
        got: u64,
        next: u64,
        max_gap: u64,
    },
    InsufficientFunds {
        need: u128,
        have: u128,
    },
    NotAuthorKey,
    Duplicate,
    ReplacementUnderpriced {
        need: u128,
        got: u128,
    },
    PoolFull {
        cap: usize,
    },
    NotReady,
    TypeNotAcceptedHere {
        type_byte: u8,
    },
}

impl SubmitError {
    pub fn tag(&self) -> &'static str {
        match self {
            SubmitError::Malformed(_) => "malformed",
            SubmitError::TooLarge { .. } => "too-large",
            SubmitError::FeeBelowRelayFloor { .. } => "fee-below-relay-floor",
            SubmitError::BadSignature => "bad-signature",
            SubmitError::NonceOutOfRange { .. } => "nonce-out-of-range",
            SubmitError::InsufficientFunds { .. } => "insufficient-funds",
            SubmitError::NotAuthorKey => "not-author-key",
            SubmitError::Duplicate => "duplicate",
            SubmitError::ReplacementUnderpriced { .. } => "replacement-underpriced",
            SubmitError::PoolFull { .. } => "pool-full",
            SubmitError::NotReady => "not-ready",
            SubmitError::TypeNotAcceptedHere { .. } => "type-not-accepted",
        }
    }

    pub fn human(&self) -> String {
        match self {
            SubmitError::Malformed(why) => format!("the bytes are not a valid transaction: {why}"),
            SubmitError::TooLarge { len, max } => {
                format!("transaction is {len} bytes, the consensus limit is {max}")
            }
            SubmitError::FeeBelowRelayFloor { fee, floor } => format!(
                "fee {fee} mile is below this node's relay floor of {floor} mile (operator policy, \
                 not a consensus rule - the consensus floor is 1 mile)"
            ),
            SubmitError::BadSignature => {
                "signature did not verify (ed25519, strict: canonical S, canonical point encoding, \
                 no small-order keys)"
                    .to_string()
            }

            SubmitError::NonceOutOfRange { got, next, max_gap } => format!(
                "nonce {got} is outside the accepted window [{next}, {}]; this account's next \
                 nonce is {next}",
                next.saturating_add(*max_gap)
            ),
            SubmitError::InsufficientFunds { need, have } => format!(
                "needs {need} mile including fee, spendable balance is {have} mile (coinbase \
                 younger than {} blocks does not count)",
                plaine_consensus::constants::COINBASE_MATURITY
            ),
            SubmitError::NotAuthorKey => {
                "this is an author announcement and its `from_pub` is not the author \
                 key this network runs, so no node will accept it and any block containing it is \
                 invalid. If you are the author, you signed with the wrong key file; compare your \
                 key's fingerprint against the one `checkpoint_getStatus` reports."
                    .to_string()
            }
            SubmitError::Duplicate => "this transaction is already known".to_string(),
            SubmitError::ReplacementUnderpriced { need, got } => format!(
                "a replacement for this (sender, nonce) needs at least {need} mile of fee, got {got}"
            ),
            SubmitError::PoolFull { cap } => {
                format!("the mempool is at its cap of {cap} transactions")
            }
            SubmitError::NotReady => {
                "the node is still syncing and cannot judge this transaction yet".to_string()
            }
            SubmitError::TypeNotAcceptedHere { type_byte } => format!(
                "transaction type 0x{type_byte:02x} cannot be submitted over RPC"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryKind {
    Coinbase,
    Transfer,
    Announcement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
    /// A transfer from the address to itself.
    SelfTransfer,
}

/// One confirmed transaction that touches an address, from that address's side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub txid: Hash32,
    pub height: u64,
    pub index: u16,
    pub time: u64,
    pub confirmations: u64,
    pub kind: HistoryKind,
    pub direction: Direction,
    /// Value credited to or debited from the address: the coinbase credit, the
    /// transfer amount, or 0 for an announcement.
    pub amount_mile: u128,
    /// Fee paid by the sender; 0 for a coinbase.
    pub fee_mile: u128,
    /// The other party of a transfer; `None` for a coinbase or an announcement.
    pub counterparty: Option<Address20>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryLookup {
    /// The node runs without `addrindex`.
    NotIndexed,
    Page {
        indexed_from: u64,
        /// Newest first.
        entries: Vec<HistoryEntry>,
        /// Position to resume from, strictly older; `None` when this page reached the
        /// oldest indexed entry.
        next_cursor: Option<(u64, u16)>,
        /// Set when some entries were skipped because their block bodies are no
        /// longer stored (pruned): the history below this height is incomplete.
        unavailable_below: Option<u64>,
    },
}

pub trait ChainView: Send + Sync {
    fn info(&self) -> ChainInfo;

    fn header_by_height(&self, height: u64) -> Option<HeaderRecord>;

    fn header_by_hash(&self, hash: &Hash32) -> Option<HeaderRecord>;

    fn invalid_reason(&self, hash: &Hash32) -> Option<String> {
        let _ = hash;
        None
    }

    fn block_by_height(&self, height: u64, verbosity: Verbosity) -> Option<BlockRecord>;

    fn block_by_hash(&self, hash: &Hash32, verbosity: Verbosity) -> Option<BlockRecord>;

    fn account(&self, addr: &Address20) -> AccountRecord;

    fn tx(&self, txid: &Hash32) -> TxLookup;

    fn emission_audit(&self, height: u64) -> Option<EmissionAudit>;

    fn author_notes(&self, cursor: NotesCursor, limit: usize) -> AuthorNotesPage;

    /// Confirmed transactions touching `addr`, newest first, strictly older than
    /// `before` when given.
    fn account_history(
        &self,
        addr: &Address20,
        before: Option<(u64, u16)>,
        limit: usize,
    ) -> HistoryLookup {
        let _ = (addr, before, limit);
        HistoryLookup::NotIndexed
    }
}

pub trait MempoolView: Send + Sync {
    fn info(&self) -> MempoolInfo;

    fn by_sender(&self, addr: &Address20) -> Vec<TxRecord>;

    fn fee_suggest(&self) -> FeeSuggestion;

    fn submit(&self, raw: &[u8]) -> Result<Hash32, SubmitError>;
}

pub trait NetView: Send + Sync {
    fn peers(&self) -> Vec<PeerInfo>;
}

pub trait StratumView: Send + Sync {
    fn sessions(&self) -> Vec<StratumSession>;
}

pub trait PolicyView: Send + Sync {
    fn checkpoint_status(&self) -> CheckpointStatus;

    fn checkpoint_link(&self) -> CheckpointLink;

    fn checkpoint_submit(&self, cp: &plaine_consensus::rules::SignedCheckpoint)
        -> CheckpointSubmit;

    fn author_key_status(&self) -> AuthorKeyStatus;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointSubmit {
    Advanced {
        height: u64,
        enforced: usize,
        enforcing: bool,
    },
    Unchanged,
    GenesisImmutable,
    Unverified,
    NotConfigured,
    Severed,
    Busy,
}

impl CheckpointSubmit {
    pub fn tag(self) -> &'static str {
        match self {
            CheckpointSubmit::Advanced { .. } => "advanced",
            CheckpointSubmit::Unchanged => "unchanged",
            CheckpointSubmit::GenesisImmutable => "genesisImmutable",
            CheckpointSubmit::Unverified => "unverified",
            CheckpointSubmit::NotConfigured => "notConfigured",
            CheckpointSubmit::Severed => "severed",
            CheckpointSubmit::Busy => "busy",
        }
    }

    pub fn advanced(self) -> bool {
        matches!(self, CheckpointSubmit::Advanced { .. })
    }
}

pub trait BudgetView: Send + Sync {
    fn budgets(&self) -> Budgets;
}

// the seam to the running node: everything the RPC can see arrives through these five traits,
// so the crate tests against a mock with no node at all.
pub struct Node {
    pub chain: std::sync::Arc<dyn ChainView>,
    pub mempool: std::sync::Arc<dyn MempoolView>,
    pub net: std::sync::Arc<dyn NetView>,
    pub stratum: std::sync::Arc<dyn StratumView>,
    pub policy: std::sync::Arc<dyn PolicyView>,
    pub budgets: std::sync::Arc<dyn BudgetView>,
}
