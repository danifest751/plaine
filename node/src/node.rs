use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Config, Network};
use crate::paths::Paths;
use crate::validator::{self, Cmd, Manager};
use crate::wire::clock::SysClock;
use crate::wire::jobs::Templates;
use crate::wire::net::{NodeView, ValidatorSink, QUEUE_ITEMS};
use crate::wire::pow::{Bits, Interp};
use crate::wire::rpcview::{
    Ask, NoteIndex, RpcBudgets, RpcChain, RpcMempool, RpcNet, RpcPolicy, RpcStratum,
};
use crate::wire::store::{new_ring, CommitSink, NodeStore, RING_MAX_BLOCKS};
use crate::wire::tip::TipCell;
use crate::{health, log};

use plaine_chain::types::ChainParams;
use plaine_consensus::constants as k;

// throttle for the periodic peers.dat write; shutdown forces one regardless.
const PEERS_SAVE_SECS: u64 = 600;

pub struct Node {
    pub tx: tokio::sync::mpsc::Sender<Cmd>,
    pub tip: TipCell,
    pub sink: Arc<CommitSink>,
    pub store: Arc<NodeStore>,
    pub notes: NoteIndex,
    validator: Option<std::thread::JoinHandle<()>>,
    net: Option<plaine_p2p::engine::host::NetNode<P2pEngine>>,
    bootstrap: Option<Bootstrapper>,
    io: Option<tokio::runtime::Runtime>,
    stratum: Option<Arc<plaine_stratum::server::StratumServer>>,
    verifier: Option<Arc<plaine_stratum::verify::ThreadPoolVerifier>>,
    pub peers: Arc<AtomicUsize>,
    pub best_known: Arc<AtomicU64>,
    peer_claim_max: Arc<AtomicU64>,
    pub verdict: Arc<Mutex<Option<crate::health::Latched>>>,
    pub shares: Arc<AtomicU64>,
    pub ibd: Arc<AtomicBool>,
    pub peer_info: Arc<Mutex<Vec<plaine_rpc::views::PeerInfo>>>,
    shares_sample: Mutex<(u64, Instant)>,
    outbound: Arc<AtomicUsize>,
    conditions_seen: Arc<AtomicU64>,
    actions_seen: Arc<AtomicU64>,
    queued_bytes: Arc<AtomicU64>,
    anchor_cells: crate::validator::AnchorCells,
    stranded: Arc<crate::validator::StrandedFlag>,
    repair_stuck: Arc<crate::health::RepairStuckFlag>,
    peers_file: std::path::PathBuf,
    last_peers_save: Mutex<Instant>,
}

type P2pEngine = plaine_p2p::sync::SyncEngine<NodeView, ValidatorSink, Interp, Bits>;

struct Bootstrapper {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Bootstrapper {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.handle.take());
    }
}

#[derive(Debug)]
pub enum StartError {
    Storage(String),
    Genesis(String),
    Chain(String),
    Pow(String),
    Bind(String),
}

impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StartError::Storage(m) => write!(f, "{m}"),
            StartError::Genesis(m) => write!(f, "{m}"),
            StartError::Chain(m) => write!(f, "{m}"),
            StartError::Pow(m) => write!(f, "{m}"),
            StartError::Bind(m) => write!(f, "{m}"),
        }
    }
}

// half the cores, capped at 8. Past 8 the share verifiers contend more than
// they help.
pub fn cpu_pool_threads() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    (cores / 2).clamp(2, 8)
}

pub fn chain_params(cfg: &Config, p: usize) -> ChainParams {
    let d = ChainParams::default();
    ChainParams {
        network: cfg.network.to_consensus(),
        author_pubkey: cfg.author_pubkey,
        authority_keys: cfg.checkpoints.keys(),
        checkpoint_threshold: cfg.checkpoint_threshold,
        max_peers: cfg.max_peers.max(1),
        // never more sync sources than peers we allow: the DoS budget math
        // (shared pool + per-source reserve) stops holding otherwise.
        max_sources: d.max_sources.min(cfg.max_peers.max(1)),
        cpu_pool_threads: p,
        genesis_bits: k::GENESIS_BITS,
        mempool: plaine_chain::types::MempoolParams {
            max_txs: cfg.mempool_max_txs,
            relay_fee_floor: cfg.relay_fee_mile,
            ..d.mempool.clone()
        },
        ..d
    }
}

#[cfg(test)]
mod param_tests {
    use super::*;
    use crate::config::Overrides;

    fn cfg() -> Config {
        crate::config::defaults(&Overrides {
            network: Some(Network::Main),
            data_dir: None,
            log_level: None,
        })
    }

    #[test]
    fn author_key_reaches_params() {
        let c = cfg();
        let p = chain_params(&c, 4);
        assert_eq!(p.author_pubkey, c.author_pubkey);

        assert_eq!(p.network, plaine_consensus::constants::Network::Main);
        assert_eq!(p.network, c.network.to_consensus());
        assert_ne!(p.author_pubkey, [0u8; 32], "the embedded default must be a real key");
        assert_eq!(p.authority_keys, c.checkpoints.keys());
        assert_eq!(p.checkpoint_threshold, c.checkpoint_threshold);
    }

    #[test]
    fn single_peer_satisfies_invariants() {
        let mut c = cfg();
        c.max_peers = 1;
        let p = chain_params(&c, 2);
        assert!(p.max_sources <= p.max_peers, "max_sources must be clamped to max_peers");
        assert!(
            p.class_reserve_is_survivable(crate::wire::pow::INTERPRETER_COST_MICROS),
            "the class reserve must survive one interpreter call per 30 s at max_peers = 1"
        );
        assert!(
            p.class_aggregate_rate_micros_per_sec() <= p.class_rate_micros_per_sec(),
            "shared + max_sources x reserve must not exceed the class cap"
        );
    }

    #[test]
    fn default_peers_satisfy_invariants() {
        let c = cfg();
        let p = chain_params(&c, 2);
        assert_eq!(p.max_peers, 128);
        assert!(p.class_reserve_is_survivable(crate::wire::pow::INTERPRETER_COST_MICROS));
        assert!(p.class_aggregate_rate_micros_per_sec() <= p.class_rate_micros_per_sec());
    }

    #[test]
    fn cpu_pool_clamped() {
        let p = cpu_pool_threads();
        assert!((2..=8).contains(&p), "P = clamp(cores/2, 2, 8), got {p}");
    }

    #[test]
    fn genesis_bits_are_compact_pow_limit() {
        let p = chain_params(&cfg(), 2);
        assert_eq!(p.genesis_bits, p.pow_limit.to_compact());
        assert_eq!(p.genesis_bits, k::GENESIS_BITS);
    }
}

pub(crate) fn chain_condition_needs_an_operator(c: &plaine_chain::Condition) -> bool {
    matches!(
        c,
        plaine_chain::Condition::ReorgTooDeepRefused { .. }
            | plaine_chain::Condition::ResyncRequired { .. }
            | plaine_chain::Condition::AnchorContradiction { .. }
            | plaine_chain::Condition::AnchorNotPersisted { .. }
            | plaine_chain::Condition::StorageFatal { .. }
    )
}

pub fn start(cfg: &Config, paths: &Paths) -> Result<Node, StartError> {
    let p = cpu_pool_threads();

    let mut store_cfg = plaine_storage::StoreConfig::new(
        paths.chain_dir.clone(),
        match cfg.network {
            Network::Main => plaine_storage::Network::Main,
        },
    );
    store_cfg.prune = cfg.prune;
    store_cfg.txindex = cfg.txindex;
    store_cfg.addrindex = cfg.addrindex;

    store_cfg.ibd_batch_blocks = Some(RING_MAX_BLOCKS as u32);

    let (mut committer, reader, report) =
        plaine_storage::open(store_cfg).map_err(|e| StartError::Storage(format!(
            "cannot open the chain database under {}: {e:?}",
            paths.chain_dir.display()
        )))?;
    if let Some(h) = report.headers_truncated_to {
        log::warn(
            "storage",
            format!("a torn header tail was truncated to height {h}; will re-sync forward"),
        );
    }
    let damaged = report.integrity.header_damage.len() + report.integrity.body_damage.len();

    if !reader.vouches_for_all_bodies() {
        let v = reader.body_vouch();
        log::security(
            "storage",
            format!(
                "{} body range(s) have no custody anchor. They are still CRC-checked on every \
                 read and every one of them satisfied its header's tx_root when it connected, so \
                 consensus uses them; they must not be advertised as vouched history.",
                v.unverifiable.len()
            ),
        );
    }

    match reader.verify_state_fingerprint() {
        Ok(()) => log::debug("storage", "state fingerprint verified"),
        // A fingerprint mismatch refuses the start. Every balance we serve derives
        // from the account table - there is no range to quarantine here.
        Err(e @ plaine_storage::StoreError::StateFingerprint { .. }) => {
            log::security(
                "storage",
                format!(
                    "{e:?} - the account table does not hash to its recorded fingerprint. \
                     This is bit rot or a code bug, and it is not confined to a range: every \
                     balance this node would serve is derived from that table. Refusing to \
                     start. Recover from a backup or re-sync this data directory."
                ),
            );
            return Err(StartError::Storage(format!("{e:?}")));
        }
        Err(e) => log::warn(
            "storage",
            format!("could not verify the state fingerprint: {e:?}. Not a clean verdict."),
        ),
    }

    if cfg.verify_frames {
        let started = Instant::now();
        let r = l2_frame_sweep(&reader);
        for seg in &r.mismatched {
            log::security(
                "storage",
                format!(
                    "L2: segment {seg}'s frame array does not match its anchor. A payload \
                     inside it was substituted at the same length with its CRC field fixed \
                     up, which is the one class L1 cannot see."
                ),
            );
        }
        log::info("storage", format!("{} in {:?}", r.line(), started.elapsed()));

        let started = Instant::now();
        let h = l3h_header_sweep(&reader);
        for (seg, height) in &h.broken {
            log::security(
                "storage",
                format!(
                    "L3-H: header segment {seg} does not link at height {height}. The header \
                     below it does not hash to the parent that header states, so at least \
                     one header in this segment is not the one this node sealed. This is \
                     the class L1 (first+last only) and L2 (body frames only) both miss, \
                     and until now it was served silently."
                ),
            );
        }
        log::info("storage", format!("{} in {:?}", h.line(), started.elapsed()));
    }

    match reader.invalid_census() {
        Ok(census) if !census.is_empty() => {
            let total: u64 = census.iter().map(|(_, n)| *n).sum();
            let by_reason = census
                .iter()
                .map(|(r, n)| format!("{n} {}", r.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            log::info(
                "storage",
                format!(
                    "{total} block(s) permanently refused: {by_reason}. The bans are durable \
                     and survive restart, which is what stops a refetch storm; there is no \
                     selective clear."
                ),
            );
        }
        Ok(_) => {}

        Err(e) => log::warn(
            "storage",
            format!("could not read the permanently-refused table: {e:?}"),
        ),
    }
    if damaged > 0 {
        log::security(
            "storage",
            format!("{damaged} damaged range(s) on disk; reads over them are refused rather than served short"),
        );
    }

    let interp = Arc::new(Interp::new().map_err(|e| StartError::Pow(format!("{e}")))?);
    let clock = Arc::new(SysClock::new());
    let ring = new_ring();
    let notes = NoteIndex::new();

    let genesis = crate::genesis::for_network(cfg.network)
        .map_err(|e| StartError::Genesis(e.to_string()))?;
    if !crate::genesis::verify_pow(&interp, &genesis.header_bytes) {
        return Err(StartError::Genesis(crate::genesis::GenesisError::PowFailed.to_string()));
    }
    // Empty store: write block 0. A populated store has to match the genesis this
    // binary carries; if it does not, it is another network's chain (checked below).
    if reader.hdr_watermark() == 0 {
        let plan = plaine_storage::BlockToCommit {
            header: &genesis.header_bytes,
            hash: genesis.block_hash,
            height: 0,
            body: &genesis.body,
            deltas: &[],
            undo: &[],
            issued_delta: plaine_consensus::emission::block_reward(0),
            chainwork: [0u8; 32],
            txids: None,
        };
        committer
            .extend(&[plan])
            .map_err(|e| StartError::Genesis(format!("{e:?}")))?;

        committer.flush().map_err(|e| StartError::Genesis(format!("{e:?}")))?;
        log::info(
            "chain",
            format!(
                "created the {} genesis at height 0, hash {}",
                cfg.network.as_str(),
                plaine_consensus::hex::encode(&genesis.block_hash)
            ),
        );
    } else {
        let found = reader
            .hash_at(0)
            .ok()
            .flatten()
            .ok_or_else(|| StartError::Genesis("the store holds no genesis".into()))?;
        if let Some(expected) = crate::genesis::expected_hash(cfg.network) {
            if found != expected {
                return Err(StartError::Genesis(
                    crate::genesis::GenesisError::WrongNetwork { found, expected }.to_string(),
                ));
            }
        }
        log::info(
            "chain",
            format!(
                "loaded the {} chain at height {}, genesis {}",
                cfg.network.as_str(),
                reader.tip().height,
                plaine_consensus::hex::encode(&found)
            ),
        );
    }

    let store = Arc::new(NodeStore::new(reader.clone(), Arc::clone(&ring)));
    let sink = Arc::new(CommitSink::new(committer, reader.clone(), Arc::clone(&ring), notes.clone()));
    let params = chain_params(cfg, p);
    let manager: Manager = plaine_chain::ChainManager::new(
        Arc::clone(&store),
        Arc::clone(&sink),
        Arc::clone(&interp),
        Arc::clone(&clock),
        params,

        Some(Box::new(|c| {
            if chain_condition_needs_an_operator(&c) {
                log::warn("chain", format!("{c:?}"))
            } else {
                log::debug("chain", format!("{c:?}"))
            }
        })),
    )
    .map_err(|e| StartError::Chain(format!("{e:?}")))?;

    let mut manager = manager;
    match reader.checkpoint_anchor() {
        Ok(None) => {}
        Ok(Some(raw)) => match plaine_consensus::checkpoint_record::decode(&raw) {
            Ok(cp) => {
                let (h, hash) = (cp.height, plaine_consensus::hex::encode(&cp.hash));
                if manager.load_anchor(&cp) {
                    log::info(
                        "chain",
                        format!(
                            "checkpoint anchor loaded from disk and re-verified: height {h}, block {hash}. Reorgs deeper than the cap are admissible only on a branch containing it."
                        ),
                    );
                } else {
                    log::security(
                        "chain",
                        format!(
                            "the checkpoint anchor on disk (height {h}) did not verify against the authority keys this node is configured with, so it was discarded and layer 2 starts shut. Expected after an authority key rotation; otherwise something else has written to the data directory. Deliver a fresh checkpoint with checkpoint_submit."
                        ),
                    );
                }
            }
            Err(e) => log::security(
                "chain",
                format!(
                    "the checkpoint anchor row in the store could not be parsed ({e}), so it was discarded and layer 2 starts shut. Nothing else is affected: the row is rewritten by the next checkpoint that advances the anchor."
                ),
            ),
        },
        Err(e) => log::warn(
            "chain",
            format!("could not read the checkpoint anchor row ({e:?}); layer 2 starts shut"),
        ),
    }
    let manager = manager;

    let tip = {
        use plaine_chain::traits::Store;
        let t = store.tip();
        let mut chainwork = [0u8; 32];
        for i in 0..4 {
            let off = 24 - i * 8;
            chainwork[off..off + 8].copy_from_slice(&t.chainwork.0[i].to_be_bytes());
        }
        TipCell::new(crate::wire::tip::TipView {
            height: t.height,
            hash: t.hash,
            time: t.time,
            chainwork,
            ..Default::default()
        })
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Cmd>(QUEUE_ITEMS as usize);
    let stratum_gen: Arc<Mutex<Option<Arc<plaine_stratum::server::StratumServer>>>> =
        Arc::new(Mutex::new(None));

    let net_sink = Arc::new(ValidatorSink::new(tx.clone(), tip.clone()));

    let view = Arc::new(NodeView::new(
        tip.clone(),
        Arc::clone(&store),
        genesis.block_hash,
        tx.clone(),
    ));
    let wanted_cell = view.wanted_bodies_cell();
    let mempool_cell = view.mempool_ids_cell();
    let anchor_cells = view.anchor_cells();
    let anchor_cells_for_node = (
        Arc::clone(&anchor_cells.0),
        Arc::clone(&anchor_cells.1),
        Arc::clone(&anchor_cells.2),
    );

    let stranded = Arc::new(crate::validator::StrandedFlag::default());
    let stranded_for_validator = Arc::clone(&stranded);
    let ibd = net_sink.ibd_flag();
    let queued_bytes = net_sink.byte_counter();

    let header_refusals = net_sink.refusal_cell();

    let header_holds = net_sink.held_cell();
    let validator = {
        let tip = tip.clone();
        let interp = Arc::clone(&interp);
        let clock = Arc::clone(&clock);
        let note = cfg.author_note.clone();
        let queued = Arc::clone(&queued_bytes);
        let gen = Arc::clone(&stratum_gen);
        std::thread::Builder::new()
            .name("plaine-validator".into())
            .spawn(move || {
                validator::run(
                    manager,
                    rx,
                    tip,
                    interp,
                    clock,
                    note,
                    queued,
                    wanted_cell,
                    mempool_cell,
                    anchor_cells,
                    header_refusals,
                    header_holds,
                    stranded_for_validator,
                    Box::new(move |h| {
                        if let Some(s) = gen.lock().expect("stratum handle").as_ref() {
                            s.notify_generation();
                        }
                        log::debug("chain", format!("tip {h}"));
                    }),
                )
            })
            .expect("spawning the validator")
    };

    let peers = Arc::new(AtomicUsize::new(0));
    let best_known = Arc::new(AtomicU64::new(0));
    let shares = Arc::new(AtomicU64::new(0));
    let peer_info = Arc::new(Mutex::new(Vec::new()));

    let p2p_cfg = plaine_p2p::config::P2pConfig {
        magic: match cfg.network {
            Network::Main => k::MAGIC_MAIN,
        },
        chain_id: cfg.network.to_consensus().chain_id(),
        port: cfg.p2p_listen.port(),
        seeds: cfg.seeds.clone(),
        authority_keys: cfg.checkpoints.keys(),
        checkpoint_threshold: cfg.checkpoint_threshold,
        genesis: genesis.block_hash,
        isolated: false,
        accept_local_addrs: cfg.accept_local_addrs,
        ..plaine_p2p::config::P2pConfig::default()
    };
    let engine = plaine_p2p::sync::SyncEngine::new(
        Arc::clone(&view),
        Arc::clone(&net_sink),
        Arc::clone(&interp),
        Arc::new(Bits::default()),
        p2p_cfg.clone(),
        seed_from_clock(),
        plaine_p2p::traits::Mono(0),
    );
    let net = plaine_p2p::engine::host::NetNode::start(
        engine,
        Arc::clone(&view),
        p2p_cfg,
        Arc::clone(&clock) as Arc<dyn plaine_p2p::traits::Clock>,
        plaine_p2p::engine::host::NetOptions {
            listen: cfg.p2p_listen,
            ticks: plaine_p2p::engine::host::TickMode::Auto,
            workers: 4,
        },
    )
    .map_err(|e| StartError::Bind(format!("cannot bind p2p on {}: {e}", cfg.p2p_listen)))?;

    let seed_port = k::PORT_P2P;
    let boot = crate::seeds::Bootstrap::new(&cfg.seeds, seed_port);
    let literals = boot.literals();
    let names = boot.resolvable_names();
    let placeholders = boot.placeholders();

    for a in &literals {
        net.dial(*a);
    }
    log::info(
        "p2p",
        format!(
            concat!(
                "listening on {} (max {} peers; {} seed address(es) dialled, ",
                "{} seed name(s) to resolve{})"
            ),
            net.local_addr(),
            cfg.max_peers,
            literals.len(),
            names,

            if placeholders > 0 {
                format!(", {placeholders} build placeholder(s) ignored")
            } else {
                String::new()
            }
        ),
    );

    match crate::peersdat::load(&paths.peers_file) {
        None => log::info("p2p", "no peers.dat yet; bootstrapping from seeds"),
        Some(bytes) => match net.net().load_addrs(&bytes) {
            Err(e) => log::warn(
                "p2p",
                format!("peers.dat ignored: {} ({e:?})", e.describe()),
            ),
            Ok(st) => log::info(
                "p2p",
                format!(
                    "peers.dat: {} address(es) restored ({} unroutable now, {} over the per-source quota, {} too old, {} already known, {} over cap)",
                    st.loaded, st.filtered, st.over_quota, st.too_old, st.duplicate, st.full
                ),
            ),
        },
    }
    if boot.is_hopeless() {
        log::warn(
            "p2p",
            concat!(
                "No usable seeds. This node has nothing to dial and will only be found by ",
                "peers that dial in. If that is not deliberate, set `p2p.seeds` in noded.toml."
            ),
        );
    }
    let bootstrap = {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let netref = Arc::clone(net.net());
        let mut boot = boot;

        let mut driver = crate::seeds::Driver::new({
            boot.settle();
            boot
        });
        for a in &literals {
            driver.pre_offered(*a);
        }
        let handle = std::thread::Builder::new()
            .name("seed-bootstrap".into())
            .spawn(move || {
                let resolver = crate::seeds::SystemResolver;
                let net = netref;
                crate::seeds::drive_with(
                    driver,
                    &resolver,
                    &|a| net.add_address(a),
                    &|| net.outbound_count(),
                    &stop2,
                );
            })
            .expect("spawning the seed bootstrap thread");
        Bootstrapper { stop, handle: Some(handle) }
    };

    let io = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("node-io")
        .build()
        .map_err(|e| StartError::Bind(format!("cannot start the node-io runtime: {e}")))?;

    let jobs = Templates::new(tx.clone(), tip.clone());
    let router = Arc::new(plaine_stratum::server::Router::new());
    let pool = Arc::new(plaine_stratum::verify::ThreadPoolVerifier::new(
        p,
        cfg.stratum_max_connections,
        Arc::clone(&interp) as Arc<dyn plaine_stratum::verify::PowHasher>,
        router.sink(),
    ));
    let verifier_handle = Arc::clone(&pool);
    let verifier: Arc<dyn plaine_stratum::verify::ShareVerifier> = pool;
    let mut server_cfg = plaine_stratum::ServerConfig::for_mode(plaine_stratum::Mode::Solo);

    server_cfg.address_hrp = k::ADDRESS_HRP.to_string();

    server_cfg.setpoint_secs = if cfg.stratum_setpoint_secs != 0 {
        cfg.stratum_setpoint_secs as f64
    } else {
        plaine_stratum::budget::effective_setpoint_secs(cfg.stratum_max_connections, p)
    };
    server_cfg.tick_ms = cfg.stratum_tick_ms;
    server_cfg.auth_deadline_ms = cfg.stratum_auth_deadline_ms;
    server_cfg.idle_evict_ms = cfg.stratum_idle_evict_ms;
    server_cfg.read_deadline_ms = cfg.stratum_read_deadline_ms;
    server_cfg.write_timeout_ms = cfg.stratum_write_timeout_ms;
    server_cfg.out_buf_cap = cfg.stratum_out_buf_cap;
    server_cfg.rates = cfg.stratum_rates;
    server_cfg.cadence = cfg.stratum_cadence;
    server_cfg.diff = cfg.stratum_diff;
    server_cfg.bans = cfg.stratum_bans;
    server_cfg.vardiff_enabled = cfg.stratum_vardiff_enabled;
    server_cfg.vardiff_fixed_diff = cfg.stratum_vardiff_fixed_diff;
    server_cfg.vardiff_max_diff = cfg.stratum_vardiff_max_diff;
    let mut caps = plaine_stratum::Caps::for_mode(plaine_stratum::Mode::Solo);
    caps.max_connections = if cfg.stratum_enforcement { cfg.stratum_max_connections } else { 0 };
    caps.max_per_ip = cfg.stratum_max_per_ip;
    caps.new_conns_per_ip_per_min = cfg.stratum_new_conns_per_ip_per_min;
    caps.global_accept_per_sec = cfg.stratum_global_accept_per_sec;
    let accept_bucket = if caps.global_accept_per_sec == 0 {
        plaine_stratum::abuse::TokenBucket::unlimited()
    } else {
        plaine_stratum::abuse::TokenBucket::new(
            caps.global_accept_per_sec as f64,
            caps.global_accept_per_sec as f64,
            0,
        )
    };
    let shared = Arc::new(plaine_stratum::Shared {
        bans: Mutex::new(plaine_stratum::abuse::BanTable::with_policy(cfg.stratum_bans)),
        e1: std::sync::Arc::new(Mutex::new(plaine_stratum::nonce::E1Allocator::new())),
        diffs: Mutex::new(plaine_stratum::abuse::DiffCache::with_policy(cfg.stratum_diff)),
        accept: Mutex::new(accept_bucket),
        jobs: jobs as Arc<dyn plaine_stratum::job::JobSource>,
        verifier,
        metrics: plaine_stratum::metrics::Metrics::default(),
        caps,
        cfg: server_cfg,
    });
    let stratum = plaine_stratum::server::StratumServer::new(shared, router);
    *stratum_gen.lock().expect("stratum handle") = Some(Arc::clone(&stratum));
    let listener = io
        .block_on(tokio::net::TcpListener::bind(cfg.stratum_listen))
        .map_err(|e| StartError::Bind(format!("cannot bind stratum on {}: {e}", cfg.stratum_listen)))?;
    let stratum_addr = listener.local_addr().unwrap_or(cfg.stratum_listen);
    {
        let s = Arc::clone(&stratum);
        io.spawn(async move {
            if let Err(e) = s.serve(listener).await {
                log::error("stratum", format!("accept loop stopped: {e}"));
            }
        });
    }
    log::info(
        "stratum",
        format!(
            "listening on {stratum_addr} (max {} connections, {p} share-verify threads, setpoint {:.0}s, hrp {})",
            cfg.stratum_max_connections,
            stratum.shared().cfg.setpoint_secs,
            k::ADDRESS_HRP
        ),
    );

    Ok(Node {
        tx,
        tip,
        sink,
        store,
        notes,
        validator: Some(validator),
        net: Some(net),
        bootstrap: Some(bootstrap),
        io: Some(io),
        stratum: Some(stratum),
        verifier: Some(verifier_handle),
        peers,
        best_known,
        peer_claim_max: Arc::new(AtomicU64::new(0)),
        verdict: Arc::new(Mutex::new(None)),
        shares,
        ibd,
        peer_info,
        shares_sample: Mutex::new((0, Instant::now())),
        outbound: Arc::new(AtomicUsize::new(0)),
        conditions_seen: Arc::new(AtomicU64::new(0)),
        actions_seen: Arc::new(AtomicU64::new(0)),
        queued_bytes,
        anchor_cells: anchor_cells_for_node,
        stranded,
        repair_stuck: Arc::new(crate::health::RepairStuckFlag::default()),
        peers_file: paths.peers_file.clone(),
        last_peers_save: Mutex::new(Instant::now()),
    })
}

// per-process seed for the p2p engine. Mixes the clock with the pid; two nodes
// started in the same instant still draw different seeds.
fn seed_from_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ (std::process::id() as u64) << 32
}

impl Node {
    pub fn rpc_views(&self, cfg: &Config) -> plaine_rpc::views::Node {
        let store = Arc::clone(&self.store);
        let notes = self.notes.clone();
        let ask = Ask::new(self.tx.clone());
        let sunset = k::CHECKPOINT_SUNSET_HEIGHT;
        plaine_rpc::views::Node {
            chain: Arc::new(RpcChain {
                tip: self.tip.clone(),
                store,
                notes,
                ask: ask.clone(),
                network: cfg.network.to_rpc(),
                pruned: cfg.prune,
                txindex: cfg.txindex,
                best_known: Arc::clone(&self.best_known),
                peers: Arc::clone(&self.peers),
                verdict: Arc::clone(&self.verdict),
            }),
            mempool: Arc::new(RpcMempool {
                ask: ask.clone(),
                tx: self.tx.clone(),
                relay_fee_mile: cfg.relay_fee_mile,
                max_txs: cfg.mempool_max_txs,
            }),
            net: Arc::new(RpcNet { peers: Arc::clone(&self.peer_info) }),
            stratum: Arc::new(RpcStratum { server: self.stratum.clone() }),
            policy: Arc::new(RpcPolicy {
                checkpoint: plaine_rpc::views::CheckpointStatus {
                    enabled: cfg.checkpoints.is_enabled(),
                    key_source: cfg.checkpoints.source(),
                    key_fingerprints: cfg.checkpoints.fingerprints(),
                    threshold: cfg.checkpoint_threshold,
                    last_anchor: None,
                    enforced_count: 0,
                    sunset_height: sunset,
                    blocks_until_sunset: Some(sunset.saturating_sub(self.tip.height())),
                    sunset_passed: self.tip.height() >= sunset,
                },
                tip_cell: self.tip.clone(),
                link: (
                    Arc::clone(&self.anchor_cells.0),
                    Arc::clone(&self.anchor_cells.1),
                    Arc::clone(&self.anchor_cells.2),
                ),
                tx: self.tx.clone(),
                checkpoints_enabled: cfg.checkpoints.is_enabled(),
                author: plaine_rpc::views::AuthorKeyStatus {
                    enabled: true,
                    key_source: cfg.author_key_source,
                    fingerprint: crate::embedded::fingerprint(&cfg.author_pubkey),
                    show_in_log: cfg.author_show_in_log,
                },
            }),
            budgets: Arc::new(RpcBudgets {
                ask,
                cpu_pool_threads: cpu_pool_threads(),
                validator_queue: Arc::clone(&self.queued_bytes),
                shares: Arc::clone(&self.shares),
            }),
        }
    }

    pub fn refresh(&self) {
        let n = self.net.as_ref().map(|net| net.peer_count()).unwrap_or(0);
        self.peers.store(n, Ordering::Relaxed);

        self.outbound.store(
            self.net.as_ref().map(|net| net.outbound_count()).unwrap_or(0),
            Ordering::Relaxed,
        );
        self.refresh_peer_info();
        self.refresh_share_rate();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // still catching up while the tip is older than the sync window. Steady-state
        // header handling differs from initial download.
        let tip_age = now.saturating_sub(self.tip.get().time);
        self.ibd.store(
            tip_age > ChainParams::default().sync_window_secs,
            Ordering::Relaxed,
        );

        self.scan_conditions(now);

        let _ = self.tx.try_send(Cmd::Tick);
    }

    fn scan_conditions(&self, now: u64) {
        let Some(net) = self.net.as_ref() else { return };
        self.stranded.tick(now);
        self.repair_stuck.tick(now);
        let seen = self.conditions_seen.load(Ordering::Relaxed);
        let (fresh, next, missed) = net.conditions_since(seen);
        if missed > 0 {
            log::warn("p2p", format!("{missed} conditions were evicted before they were read"));
        }
        if fresh.is_empty() {
            return;
        }
        for cond in &fresh {
            match cond {
                // Fork body catch-up is normal progress, not a fault. Show a calm
                // one-liner at info and keep the struct for a debug session.
                plaine_p2p::traits::Condition::ForkBodiesWanted { applied, missing, .. } => {
                    log::info(
                        "p2p",
                        format!(
                            "syncing  height {}  {} block(s) to go",
                            log::thousands(*applied),
                            missing
                        ),
                    );
                    log::debug("p2p", format!("{cond:?}"));
                }
                _ => log::warn("p2p", format!("{cond:?}")),
            }
            Self::stamp_condition(&self.stranded, &self.repair_stuck, now, cond);
        }
        self.conditions_seen.store(next, Ordering::Relaxed);
    }

    fn stamp_condition(
        stranded: &crate::validator::StrandedFlag,
        repair_stuck: &crate::health::RepairStuckFlag,
        now: u64,
        cond: &plaine_p2p::traits::Condition,
    ) {
        if let plaine_p2p::traits::Condition::HeaderRepairStuck {
            height,
            why,
            repeats,
            our_tip,
        } = cond
        {
            repair_stuck.note(
                now,
                crate::health::TransportRepairStuck {
                    height: *height,
                    why,
                    repeats: *repeats,
                    our_tip: *our_tip,
                },
            );
        }
        if let plaine_p2p::traits::Condition::StrandedBeyondReorgCap {
            our_tip,
            their_tip,
            depth,
        } = cond
        {
            stranded.note_with(
                now,
                crate::health::TransportStranded {
                    our_tip: *our_tip,
                    their_tip: *their_tip,
                    depth: *depth,
                    cap: ChainParams::default().max_reorg_depth,
                },
            );
        }
    }

    fn refresh_peer_info(&self) {
        let Some(net) = self.net.as_ref() else { return };
        let rows = net.net().peer_rows();

        let claims: Vec<u64> = rows.iter().map(|r| r.best_height).collect();
        // best_known is the DoS-resistant second-highest claim; peer_claim_max keeps
        // the raw max only to tell a following peer from a frozen one.
        self.best_known
            .store(health::backed_height(&claims), Ordering::Relaxed);

        self.peer_claim_max
            .store(claims.iter().copied().max().unwrap_or(0), Ordering::Relaxed);
        let out = rows
            .into_iter()
            .map(|r| plaine_rpc::views::PeerInfo {
                id: r.id.0,
                addr: display_ip(&r.ip),
                outbound: r.outbound,
                connected_secs: r.connected_ms / 1_000,
                best_height: r.best_height,
                bytes_recv: r.bytes_recv,
                bytes_sent: r.bytes_sent,
                misbehaviour: r.misbehaviour,
                user_agent: r.user_agent,
            })
            .collect();
        *self.peer_info.lock().expect("peer info") = out;
    }

    fn refresh_share_rate(&self) {
        let Some(s) = self.stratum.as_ref() else { return };
        let now = Instant::now();
        let total = plaine_stratum::metrics::Metrics::get(&s.shared().metrics.shares_accepted);
        let mut last = self.shares_sample.lock().expect("share sample");
        let dt = now.saturating_duration_since(last.1).as_secs_f64();
        if dt >= 1.0 {
            let rate = ((total.saturating_sub(last.0)) as f64 / dt).round() as u64;
            self.shares.store(rate, Ordering::Relaxed);
            *last = (total, now);
        }
    }

    pub fn save_peers(&self, force: bool) -> bool {
        let Some(net) = self.net.as_ref() else { return false };
        {
            let mut last = self.last_peers_save.lock().expect("peers save clock");
            if !force && last.elapsed() < Duration::from_secs(PEERS_SAVE_SECS) {
                return false;
            }
            *last = Instant::now();
        }
        let bytes = net.net().addr_snapshot();
        crate::peersdat::store(&self.peers_file, &bytes)
    }

    pub fn sweep_headers(&self, sweeper: &mut plaine_storage::sweep::HeaderSweeper) {
        let r = sweeper.step(self.store.reader());
        if r.is_clean() {
            return;
        }
        for (seg, height) in &r.broken {
            log::security(
                "storage",
                format!(
                    "L3-H: header segment {seg} does not link at height {height}. The header below it does not hash to the parent that header states, so at least one header in this segment is not the one this node sealed. {}",
                    r.line()
                ),
            );
        }
        if r.broken.is_empty() {
            // A no-verdict periodic sweep is not a problem a healthy node needs to
            // see; keep the detail for a debug session.
            log::debug("storage", r.line());
        }
    }

    pub fn beat(&self, tracker: &mut health::Tracker, uptime: u64) -> health::Verdict {
        self.save_peers(false);
        let t = self.tip.get();
        let n = self.peers.load(Ordering::Relaxed);

        let outbound = self.outbound.load(Ordering::Relaxed);
        if n > 0 && outbound == 0 {
            log::warn(
                "p2p",
                format!(
                    "{n} peer(s) connected and zero outbound. A sync peer can only be \
                     designated from an outbound connection, so this node can pull a \
                     chain from none of them - it will accept blocks pushed to it and \
                     nothing else. Check that outbound connections are allowed out of \
                     this box."
                ),
            );
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let tip_age = now.saturating_sub(t.time);

        if let Some(net) = self.net.as_ref() {
            if log::enabled(crate::config::LogLevel::Debug) {
                let (fresh, next, missed) =
                    net.actions_since(self.actions_seen.load(Ordering::Relaxed));
                if missed > 0 {
                    log::debug(
                        "p2p",
                        format!("{missed} engine actions were evicted before they were read"),
                    );
                }
                for a in &fresh {
                    log::debug("p2p", format!("{a:?}"));
                }
                self.actions_seen.store(next, Ordering::Relaxed);
            }

            let _ = net;
        }

        if let Some(s) = self.stratum.as_ref() {
            let m = &s.shared().metrics;
            use plaine_stratum::metrics::Metrics as M;

            let saturated = s.shared().bans.lock().map(|b| b.saturated).unwrap_or(0);
            let counts = StratumCounts {
                conns: s.live_connections(),
                accepted: M::get(&m.shares_accepted),
                submitted: M::get(&m.shares_submitted),
                verified: M::get(&m.shares_verified),
                blocks: M::get(&m.blocks_found),
                stale: M::get(&m.rej_stale),
                duplicate: M::get(&m.rej_duplicate),
                low_difficulty: M::get(&m.rej_low_difficulty),
                out_of_slice: M::get(&m.rej_out_of_slice),
                unknown_job: M::get(&m.rej_unknown_job),
                bad_json: M::get(&m.bad_json),
                keepalives: M::get(&m.keepalives),
                bans: M::get(&m.bans),
                saturated,
                internal_errors: M::get(&m.verify_internal_errors),
                throttled: M::get(&m.rej_throttled),
                server_busy: M::get(&m.rej_server_busy),
                verify_queue: self
                    .verifier
                    .as_ref()
                    .map_or(0, |v| v.queue_len()),
                revocations: s.revocations_published(),
                closed_idle: M::get(&m.closed_idle),
                closed_auth_timeout: M::get(&m.closed_auth_timeout),
                closed_slow_client: M::get(&m.closed_slow_client),
                closed_line_flood: M::get(&m.closed_line_flood),
                closed_slice_revoked: M::get(&m.closed_slice_revoked),
            };
            if counts.worth_printing() {
                log::info("stratum", stratum_line_brief(&counts));
                log::debug("stratum", stratum_line(&counts));
            }
        }

        let branch = Ask::new(self.tx.clone())
            .ask(validator::Query::Branch, Duration::from_millis(2_000));
        let best_known_height =
            health::best_known(n, self.best_known.load(Ordering::Relaxed), t.height);

        if let Some(b) = best_known_height {
            tracker.observe_gap(b.saturating_sub(t.height), uptime);
        }

        tracker.observe_top_claim(self.peer_claim_max.load(Ordering::Relaxed), uptime);

        let idle_secs = tracker.idle_secs_at(t.height, uptime);
        let obs = health::Observation {
            branch,
            transport_stranded: self.stranded.report(),
            transport_repair_stuck: self.repair_stuck.report(),
            height: t.height,
            best_known_height,
            tip_age_secs: tip_age,
            idle_secs,
            gap_idle_secs: tracker.gap_idle_secs(uptime),
            peer_height_idle_secs: tracker.peer_height_idle_secs(uptime),
            peers: n,
            started: true,
            blocks_per_sec: tracker.blocks_per_sec(uptime),
            mempool_txs: t.mempool_txs,
        };
        tracker.observe(obs.height, uptime);
        let v = health::assess(&obs);
        health::emit(&obs, &v, uptime);

        *self.verdict.lock().expect("verdict cell") =
            Some(health::Latched { obs, at: Instant::now() });
        v
    }

    pub fn shutdown(mut self) {
        if let Some(mut b) = self.bootstrap.take() {
            b.stop();
            log::info("shutdown", "seed bootstrap stopped");
        }
        if let Some(s) = self.stratum.take() {
            s.stop();
            log::info("shutdown", "stratum: miners told to disconnect");
        }

        if self.save_peers(true) {
            log::info("shutdown", "address book written to peers.dat");
        }
        if let Some(net) = self.net.take() {
            let _ = net.shutdown();
            log::info("shutdown", "p2p: peers closed, engine joined");
        }

        let _ = self.tx.blocking_send(Cmd::Stop);
        if let Some(v) = self.validator.take() {
            let _ = v.join();
            log::info("shutdown", "validator joined");
        }

        let unsealed = self.store.ring().read().map(|g| g.len()).unwrap_or(0);
        match self.sink.flush() {
            Ok(()) => {
                let s = self.sink.stats();
                log::info(
                    "shutdown",
                    format!(
                        "storage sealed and fsynced: {unsealed} unsealed block(s) made durable. This run wrote {} block(s) and {} reorg(s); the ring's dual bound forced {} extra seal(s).",
                        s.blocks, s.reorgs, s.forced_seals
                    ),
                );
            }
            Err(e) => log::error("shutdown", format!("final flush failed: {e:?}")),
        }
        if let Some(io) = self.io.take() {
            io.shutdown_timeout(Duration::from_millis(2_000));
            log::info("shutdown", "node-io runtime stopped");
        }
    }
}

fn display_ip(ip: &[u8; 16]) -> String {
    // v4-mapped (::ffff:a.b.c.d) prints as a dotted quad, not a v6 literal - peer
    // rows read the way an operator expects.
    const V4_PREFIX: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];
    if ip[..12] == V4_PREFIX {
        return format!("{}.{}.{}.{}", ip[12], ip[13], ip[14], ip[15]);
    }
    std::net::Ipv6Addr::from(*ip).to_string()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StratumCounts {
    pub conns: usize,
    pub accepted: u64,
    pub submitted: u64,
    pub verified: u64,
    pub blocks: u64,
    pub stale: u64,
    pub duplicate: u64,
    pub low_difficulty: u64,
    pub out_of_slice: u64,
    pub unknown_job: u64,
    pub bad_json: u64,
    pub keepalives: u64,
    pub bans: u64,
    pub saturated: u64,
    pub internal_errors: u64,
    pub throttled: u64,
    pub server_busy: u64,
    pub verify_queue: usize,
    pub revocations: u64,
    pub closed_idle: u64,
    pub closed_auth_timeout: u64,
    pub closed_slow_client: u64,
    pub closed_line_flood: u64,
    pub closed_slice_revoked: u64,
}

impl StratumCounts {
    pub fn worth_printing(&self) -> bool {
        self.conns > 0
            || self.submitted > 0
            || self.bad_json > 0
            || self.bans > 0
            || self.saturated > 0

            || self.internal_errors > 0
            || self.revocations > 0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct L2Sweep {
    pub verified: u64,
    pub mismatched: Vec<u32>,
    pub unjudged: u64,
    pub range: (u32, u32),
}

impl L2Sweep {
    pub fn line(&self) -> String {
        format!(
            "L2 frame sweep over segments {}..={}: {} verified, {} MISMATCHED, {} could not \
             be judged at this tier (no anchor or no identity). \"Could not be judged\" is \
             not \"clean\".",
            self.range.0,
            self.range.1,
            self.verified,
            self.mismatched.len(),
            self.unjudged
        )
    }
}

pub fn l2_frame_sweep(reader: &plaine_storage::StoreReader) -> L2Sweep {
    let first = plaine_storage::seg_of(reader.prune_floor());
    let last = plaine_storage::seg_of(reader.tip().height).max(first);
    let mut out = L2Sweep { range: (first, last), ..Default::default() };
    for seg in first..=last {
        match reader.verify_segment_frames(seg) {
            Ok(Some(true)) => out.verified += 1,
            Ok(Some(false)) => out.mismatched.push(seg),
            Ok(None) => out.unjudged += 1,
            Err(_) => out.unjudged += 1,
        }
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct L3HSweep {
    pub links: u64,
    pub broken: Vec<(u32, u64)>,
    pub unjudged: u64,
    pub range: (u32, u32),
}

impl L3HSweep {
    pub fn line(&self) -> String {
        format!(
            "L3-H header linkage sweep over segments {}..={}: {} links verified, {} BROKEN, \
             {} segment(s) could not be judged at this tier (not sealed, or already named \
             damaged by L1). \"Could not be judged\" is not \"clean\", and neither is a \
             clean L2 line above this one - L2 reads no interior header at all.",
            self.range.0,
            self.range.1,
            self.links,
            self.broken.len(),
            self.unjudged
        )
    }
}

pub fn l3h_header_sweep(reader: &plaine_storage::StoreReader) -> L3HSweep {
    let first = plaine_storage::seg_of(reader.prune_floor());
    let last = plaine_storage::seg_of(reader.tip().height).max(first);
    let mut out = L3HSweep { range: (first, last), ..Default::default() };
    for seg in first..=last {
        match reader.verify_segment_headers(seg) {
            Ok(Some(n)) => out.links += n,
            Ok(None) => out.unjudged += 1,
            Err(plaine_storage::StoreError::LinkageBroken { height, .. }) => {
                out.broken.push((seg, height))
            }

            Err(_) => out.unjudged += 1,
        }
    }
    out
}

// The short form for the default log: only what a pool operator watches at a
// glance. The full counter set lives in stratum_line, shown at debug.
pub fn stratum_line_brief(c: &StratumCounts) -> String {
    let rejected = c.submitted.saturating_sub(c.accepted);
    format!(
        "conns {}  shares {} accepted  {} rejected  blocks {}",
        c.conns, c.accepted, rejected, c.blocks
    )
}

pub fn stratum_line(c: &StratumCounts) -> String {
    format!(
        "conns {}  shares {}/{} accepted  verified {}  blocks {}  \
         verify-queue {}  \
         rejected internal-error {} throttled {} server-busy {} \
         stale {} dup {} lowdiff {} slice {} unknown-job {}  \
         bad-json {}  keepalive {}  bans {}  ban-table-saturated {}  \
         revocations {}  \
         closed idle {} auth-timeout {} slow-client {} line-flood {} slice-revoked {}",
        c.conns,
        c.accepted,
        c.submitted,
        c.verified,
        c.blocks,
        c.verify_queue,
        c.internal_errors,
        c.throttled,
        c.server_busy,
        c.stale,
        c.duplicate,
        c.low_difficulty,
        c.out_of_slice,
        c.unknown_job,
        c.bad_json,
        c.keepalives,
        c.bans,
        c.saturated,
        c.revocations,
        c.closed_idle,
        c.closed_auth_timeout,
        c.closed_slow_client,
        c.closed_line_flood,
        c.closed_slice_revoked,
    )
}

#[cfg(test)]
mod stratum_line_tests {
    use super::{stratum_line, StratumCounts};

    fn distinct() -> StratumCounts {
        StratumCounts {
            conns: 2,
            accepted: 3,
            submitted: 5,
            verified: 7,
            blocks: 11,
            stale: 13,
            duplicate: 17,
            low_difficulty: 19,
            out_of_slice: 23,
            unknown_job: 29,
            bad_json: 31,
            keepalives: 37,
            bans: 41,
            saturated: 43,
            internal_errors: 47,
            throttled: 53,
            server_busy: 59,
            verify_queue: 61,
            revocations: 67,
            closed_idle: 71,
            closed_auth_timeout: 73,
            closed_slow_client: 79,
            closed_line_flood: 83,
            closed_slice_revoked: 89,
        }
    }

    #[test]
    fn line_carries_flood_counters() {
        let line = stratum_line(&distinct());
        for (label, want) in [
            ("verified", 7u64),
            ("bad-json", 31),
            ("keepalive", 37),
            ("bans", 41),
            ("ban-table-saturated", 43),
        ] {
            assert!(
                line.contains(&format!("{label} {want}")),
                "the line must name {label}: {line}"
            );
        }
    }

    #[test]
    fn line_carries_prior_counters() {
        let line = stratum_line(&distinct());
        for (label, want) in [
            ("conns", 2u64),
            ("blocks", 11),
            ("stale", 13),
            ("dup", 17),
            ("lowdiff", 19),
            ("slice", 23),
            ("unknown-job", 29),
        ] {
            assert!(line.contains(&format!("{label} {want}")), "lost {label}: {line}");
        }
        assert!(line.contains("shares 3/5 accepted"), "{line}");
    }

    #[test]
    fn line_carries_backpressure_classes() {
        let line = stratum_line(&distinct());
        for (label, want) in [
            ("internal-error", 47u64),
            ("throttled", 53),
            ("server-busy", 59),
            ("verify-queue", 61),
            ("revocations", 67),
        ] {
            assert!(
                line.contains(&format!("{label} {want}")),
                "the line must name {label}: {line}"
            );
        }
    }

    #[test]
    fn our_failure_precedes_miner_blame() {
        let line = stratum_line(&distinct());
        let ours = line.find("internal-error").expect("internal-error on the line");
        for label in ["stale ", "dup ", "lowdiff ", "slice ", "unknown-job "] {
            let theirs = line.find(label).unwrap_or_else(|| panic!("{label} on the line"));
            assert!(ours < theirs, "`{label}` precedes internal-error: {line}");
        }
    }

    #[test]
    fn line_names_close_reasons() {
        let line = stratum_line(&distinct());
        for (label, want) in [
            ("idle", 71u64),
            ("auth-timeout", 73),
            ("slow-client", 79),
            ("line-flood", 83),
            ("slice-revoked", 89),
        ] {
            assert!(
                line.contains(&format!("{label} {want}")),
                "the line must name {label}: {line}"
            );
        }

        let g = line.find("closed idle").expect("the group is labelled");
        assert!(g > line.find("revocations").expect("revocations"), "{line}");
    }

    #[test]
    fn close_reasons_after_refusals() {
        let line = stratum_line(&distinct());
        let closes = line.find("closed idle").expect("closes");
        for label in ["internal-error", "throttled", "server-busy", "bans "] {
            let f = line.find(label).unwrap_or_else(|| panic!("{label}: {line}"));
            assert!(f < closes, "`{label}` must precede the close reasons: {line}");
        }
    }

    #[test]
    fn heartbeat_reads_close_counters() {
        let whole = include_str!("node.rs");
        let body = whole.split("mod stratum_line_tests").next().expect("the module body");
        for field in [
            "closed_idle",
            "closed_auth_timeout",
            "closed_slow_client",
            "closed_line_flood",
            "closed_slice_revoked",
        ] {
            assert!(
                body.contains(&format!("{field}: M::get(&m.{field})")),
                "`{field}` is no longer read from the stratum metrics at the heartbeat"
            );
        }
    }

    #[test]
    fn line_is_single_line() {
        assert!(!stratum_line(&distinct()).contains('\n'));
    }

    #[test]
    fn prints_under_attack_with_no_conns() {
        assert!(
            !StratumCounts::default().worth_printing(),
            "an idle node stays quiet"
        );
        for c in [
            StratumCounts { bans: 1, ..Default::default() },
            StratumCounts { bad_json: 1, ..Default::default() },
            StratumCounts { saturated: 1, ..Default::default() },
            StratumCounts { internal_errors: 1, ..Default::default() },
        ] {
            assert!(
                c.worth_printing(),
                "zero connections and zero submissions is what a refused flood \
                 looks like with nobody connected: {c:?}"
            );
        }
    }
}

#[cfg(test)]
mod peer_display_tests {
    use super::display_ip;

    #[test]
    fn v4_mapped_reads_as_v4() {
        let mut ip = [0u8; 16];
        ip[10] = 0xff;
        ip[11] = 0xff;
        ip[12..].copy_from_slice(&[127, 0, 0, 1]);
        assert_eq!(display_ip(&ip), "127.0.0.1");
    }

    #[test]
    fn v6_not_mangled() {
        let mut ip = [0u8; 16];
        ip[15] = 1;
        assert_eq!(display_ip(&ip), "::1");
    }
}

#[cfg(test)]
mod condition_stamping {
    use super::Node;
    use plaine_p2p::traits::Condition;

    const NOW: u64 = 1_800_000_000;

    fn cells() -> (crate::validator::StrandedFlag, crate::health::RepairStuckFlag) {
        (crate::validator::StrandedFlag::default(), crate::health::RepairStuckFlag::default())
    }

    #[test]
    fn stuck_repair_reaches_health_cell() {
        let (s, r) = cells();
        s.tick(NOW);
        r.tick(NOW);
        Node::stamp_condition(
            &s,
            &r,
            NOW,
            &Condition::HeaderRepairStuck {
                height: 414,
                why: "the chain does not hold the parent",
                repeats: 3,
                our_tip: 424,
            },
        );
        assert_eq!(
            r.report(),
            Some(crate::health::TransportRepairStuck {
                height: 414,
                why: "the chain does not hold the parent",
                repeats: 3,
                our_tip: 424,
            }),
            "a stuck-repair condition must reach the health cell that assess reads"
        );
    }

    #[test]
    fn conditions_use_separate_cells() {
        let (s, r) = cells();
        s.tick(NOW);
        r.tick(NOW);
        Node::stamp_condition(
            &s,
            &r,
            NOW,
            &Condition::StrandedBeyondReorgCap { our_tip: 424, their_tip: 495, depth: 11 },
        );
        assert!(s.report().is_some(), "the stranded cell was not stamped");
        assert_eq!(r.report(), None, "a stranding stamped the repair cell");
        Node::stamp_condition(
            &s,
            &r,
            NOW,
            &Condition::HeaderRepairStuck {
                height: 414,
                why: "the chain does not hold the parent",
                repeats: 3,
                our_tip: 424,
            },
        );
        assert!(r.report().is_some(), "the repair cell was not stamped");
        assert!(s.report().is_some(), "stamping the repair cell cleared the stranded one");
    }

    #[test]
    fn unrelated_condition_stamps_nothing() {
        let (s, r) = cells();
        s.tick(NOW);
        r.tick(NOW);
        Node::stamp_condition(&s, &r, NOW, &Condition::BodyUnavailable { height: 9 });
        assert_eq!(s.report(), None, "an unrelated condition stamped the stranded cell");
        assert_eq!(r.report(), None, "an unrelated condition stamped the repair cell");
    }
}

#[cfg(test)]
mod chain_condition_visibility {
    use super::chain_condition_needs_an_operator as needs;
    use plaine_chain::traits::SinkError;
    use plaine_chain::Condition;

    #[test]
    fn failed_anchor_write_is_promoted() {
        assert!(
            needs(&Condition::AnchorNotPersisted {
                height: 5_000,
                err: SinkError::Invalid("disk full"),
            }),
            "a failed anchor write must be promoted, not left at debug"
        );
    }

    #[test]
    fn promoted_conditions_stay_promoted() {
        assert!(needs(&Condition::ReorgTooDeepRefused { depth: 40, our_tip: 100, their_tip: 140, fork_height: 60 }));
        assert!(needs(&Condition::ResyncRequired { fork_height: 3, replay_floor: 9 }));
        assert!(needs(&Condition::AnchorContradiction { height: 7, hash: [1u8; 32] }));
        assert!(needs(&Condition::StorageFatal { detail: "torn" }));
    }

    #[test]
    fn ordinary_conditions_stay_debug() {
        assert!(!needs(&Condition::DeepReplay { from: 1, to: 9, blocks: 8 }));
        assert!(!needs(&Condition::ReorgOverlayExhausted { accounts: 4 }));
        assert!(!needs(&Condition::BranchInvalidAt { height: 2, hash: [0u8; 32] }));
    }
}
