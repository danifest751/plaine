use std::net::SocketAddr;
use std::path::PathBuf;

use plaine_consensus::constants as k;
use plaine_rpc::methods::edit_distance;
use plaine_stratum::limits::{BanPolicy, Cadence, DiffPolicy, RatePolicy};
use plaine_stratum::Caps;

use crate::embedded;
use crate::paths::{expand_tilde, Paths};
use crate::toml::{self, Diagnostic, Document, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Main,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        "main"
    }

    pub fn to_rpc(self) -> plaine_rpc::Network {
        plaine_rpc::Network::Main
    }

    pub fn to_consensus(self) -> plaine_consensus::constants::Network {
        plaine_consensus::constants::Network::Main
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<LogLevel> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(LogLevel::Error),
            "warn" | "warning" => Some(LogLevel::Warn),
            "info" => Some(LogLevel::Info),
            "debug" => Some(LogLevel::Debug),
            "trace" => Some(LogLevel::Trace),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN ",
            LogLevel::Info => "INFO ",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.label().trim_end()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointPolicy {
    Embedded {
        key: [u8; 32],
        placeholder: bool,
    },

    Configured {
        keys: Vec<[u8; 32]>,
    },
    Disabled,
}

impl CheckpointPolicy {
    pub fn keys(&self) -> Vec<[u8; 32]> {
        match self {
            CheckpointPolicy::Embedded { key, .. } => vec![*key],
            CheckpointPolicy::Configured { keys } => keys.clone(),
            CheckpointPolicy::Disabled => Vec::new(),
        }
    }

    pub fn fingerprints(&self) -> Vec<String> {
        self.keys().iter().map(embedded::fingerprint).collect()
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, CheckpointPolicy::Disabled)
    }

    pub fn source(&self) -> plaine_rpc::views::KeySource {
        match self {
            CheckpointPolicy::Configured { .. } => plaine_rpc::views::KeySource::Config,
            _ => plaine_rpc::views::KeySource::Embedded,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            CheckpointPolicy::Embedded { placeholder, .. } => format!(
                "ON, 1 key (embedded official{}, fp {}), threshold 1",
                if *placeholder { " PLACEHOLDER" } else { "" },
                self.fingerprints().join(",")
            ),

            CheckpointPolicy::Configured { keys } => format!(
                "ON, {} key(s) from config{} (fp {}), threshold set below",
                keys.len(),
                if keys.iter().any(|k| embedded::placeholder_role(k).is_some()) {
                    " including a BUILD PLACEHOLDER"
                } else {
                    ""
                },
                self.fingerprints().join(",")
            ),
            CheckpointPolicy::Disabled => {
                "OFF - checkpoints.enabled = false. This node accepts deep reorgs that a \
                 checkpointed node rejects."
                    .to_string()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Cli,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::File => "file",
            Source::Cli => "cli",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub network: Network,
    pub data_dir: PathBuf,
    pub prune: bool,
    pub txindex: bool,
    pub addrindex: bool,
    pub verify_frames: bool,
    pub p2p_listen: SocketAddr,
    pub max_peers: usize,
    pub seeds: Vec<String>,
    pub use_embedded_seeds: bool,
    pub accept_local_addrs: bool,
    pub stratum_listen: SocketAddr,
    pub stratum_max_connections: usize,
    pub stratum_enforcement: bool,
    pub stratum_max_per_ip: usize,
    pub stratum_new_conns_per_ip_per_min: u32,
    pub stratum_global_accept_per_sec: u32,
    pub stratum_bans: plaine_stratum::limits::BanPolicy,
    pub stratum_rates: plaine_stratum::limits::RatePolicy,
    pub stratum_diff: plaine_stratum::limits::DiffPolicy,
    pub stratum_cadence: plaine_stratum::limits::Cadence,
    pub stratum_vardiff_enabled: bool,
    pub stratum_vardiff_fixed_diff: u64,
    pub stratum_vardiff_max_diff: u64,
    pub stratum_setpoint_secs: u64,
    pub stratum_auth_deadline_ms: u64,
    pub stratum_idle_evict_ms: u64,
    pub stratum_read_deadline_ms: u64,
    pub stratum_write_timeout_ms: u64,
    pub stratum_out_buf_cap: usize,
    pub stratum_tick_ms: u64,
    pub rpc_listen: SocketAddr,
    pub rpc_token: Option<String>,
    pub relay_fee_mile: u128,
    pub mempool_max_txs: usize,
    pub author_note: Vec<u8>,
    pub checkpoints: CheckpointPolicy,
    pub checkpoint_threshold: usize,
    pub author_pubkey: [u8; 32],
    pub author_key_source: plaine_rpc::views::KeySource,
    pub author_key_placeholder: bool,
    pub author_show_in_log: bool,
    pub log_level: LogLevel,
    pub provenance: Vec<(String, String, Source)>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Str,
    Int,
    Bool,
    StrArray,
    IntArray,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Str => "a string",
            Kind::Int => "an integer",
            Kind::Bool => "a boolean (true or false)",
            Kind::StrArray => "an array of strings",
            Kind::IntArray => "an array of integers",
        }
    }
}

struct Field {
    section: &'static str,
    key: &'static str,
    kind: Kind,
}

const SCHEMA: &[Field] = &[
    Field { section: "node", key: "network", kind: Kind::Str },
    Field { section: "node", key: "data_dir", kind: Kind::Str },
    Field { section: "node", key: "prune", kind: Kind::Bool },
    Field { section: "node", key: "txindex", kind: Kind::Bool },
    Field { section: "node", key: "addrindex", kind: Kind::Bool },
    Field { section: "node", key: "verify_frames", kind: Kind::Bool },
    Field { section: "p2p", key: "listen", kind: Kind::Str },
    Field { section: "p2p", key: "max_peers", kind: Kind::Int },
    Field { section: "p2p", key: "seeds", kind: Kind::StrArray },
    Field { section: "p2p", key: "use_embedded_seeds", kind: Kind::Bool },
    Field { section: "p2p", key: "accept_local_addrs", kind: Kind::Bool },
    Field { section: "stratum", key: "listen", kind: Kind::Str },
    Field { section: "stratum", key: "max_connections", kind: Kind::Int },
    Field { section: "stratum", key: "enforcement", kind: Kind::Bool },
    Field { section: "stratum", key: "max_per_ip", kind: Kind::Int },
    Field { section: "stratum", key: "new_conns_per_ip_per_min", kind: Kind::Int },
    Field { section: "stratum", key: "global_accept_per_sec", kind: Kind::Int },
    Field { section: "stratum", key: "bans_enabled", kind: Kind::Bool },
    Field { section: "stratum", key: "ban_threshold", kind: Kind::Int },
    Field { section: "stratum", key: "ban_soft_cap", kind: Kind::Int },
    Field { section: "stratum", key: "ban_decay_secs", kind: Kind::Int },
    Field { section: "stratum", key: "ban_base_secs", kind: Kind::Int },
    Field { section: "stratum", key: "ban_ladder_factor", kind: Kind::Int },
    Field { section: "stratum", key: "ban_max_secs", kind: Kind::Int },
    Field { section: "stratum", key: "ban_table_entries", kind: Kind::Int },
    Field { section: "stratum", key: "garbage_throttle_secs", kind: Kind::Int },
    Field { section: "stratum", key: "submit_rate_per_sec", kind: Kind::Int },
    Field { section: "stratum", key: "submit_burst", kind: Kind::Int },
    Field { section: "stratum", key: "line_rate_per_sec", kind: Kind::Int },
    Field { section: "stratum", key: "line_burst", kind: Kind::Int },
    Field { section: "stratum", key: "auth_deadline_secs", kind: Kind::Int },
    Field { section: "stratum", key: "idle_evict_secs", kind: Kind::Int },
    Field { section: "stratum", key: "read_deadline_secs", kind: Kind::Int },
    Field { section: "stratum", key: "write_timeout_secs", kind: Kind::Int },
    Field { section: "stratum", key: "out_buf_cap_bytes", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_setpoint_secs", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_start_diff", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_min_diff", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_tick_secs", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_retarget_gate_shares", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_retarget_gate_secs", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_warmup_shares", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_warmup_gate_shares", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_mature_shares", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_max_step", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_enabled", kind: Kind::Bool },
    Field { section: "stratum", key: "vardiff_fixed_diff", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_max_diff", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_dead_zone_pct", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_mature_zone_pct", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_fast_escape_pct", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_silence_slack", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_dsps_tau_fast_secs", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_dsps_tau_mid_secs", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_dsps_tau_slow_secs", kind: Kind::Int },
    Field { section: "stratum", key: "vardiff_ladder", kind: Kind::IntArray },
    Field { section: "stratum", key: "diff_cache_entries", kind: Kind::Int },
    Field { section: "stratum", key: "diff_cache_ttl_secs", kind: Kind::Int },
    Field { section: "stratum", key: "reconnect_storm_per_min", kind: Kind::Int },
    Field { section: "stratum", key: "reconnect_storm_window_secs", kind: Kind::Int },
    Field { section: "rpc", key: "listen", kind: Kind::Str },
    Field { section: "rpc", key: "token", kind: Kind::Str },
    Field { section: "mempool", key: "relay_fee_mile", kind: Kind::Int },
    Field { section: "mempool", key: "max_txs", kind: Kind::Int },
    Field { section: "mining", key: "author_note", kind: Kind::Str },
    Field { section: "checkpoints", key: "enabled", kind: Kind::Bool },
    Field { section: "checkpoints", key: "threshold", kind: Kind::Int },
    Field { section: "checkpoints", key: "keys", kind: Kind::StrArray },
    Field { section: "author", key: "pubkey", kind: Kind::Str },
    Field { section: "author", key: "show_in_log", kind: Kind::Bool },
    Field { section: "log", key: "level", kind: Kind::Str },
];

pub const STRATUM_CONNECTION_CEILING: usize =
    plaine_stratum::limits::SOLO_MAX_CONNECTIONS_CEILING;

#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub network: Option<Network>,
    pub data_dir: Option<PathBuf>,
    pub log_level: Option<LogLevel>,
}

pub fn load(path: &str, src: &str, overrides: &Overrides) -> Result<Config, Diagnostic> {
    // Fail closed, in order. An unknown key or section stops startup before any
    // value is read; a misspelled setting is never silently ignored.
    let doc = toml::parse(path, src)?;
    check_unknown(path, &doc)?;
    check_types(path, &doc)?;
    build(path, &doc, overrides)
}

pub fn defaults(overrides: &Overrides) -> Config {
    load("<defaults>", "", overrides)
        .expect("the built-in defaults must always validate; this is a bug, not a config error")
}

fn field(section: &str, key: &str) -> Option<&'static Field> {
    SCHEMA.iter().find(|f| f.section == section && f.key == key)
}

fn check_unknown(path: &str, doc: &Document) -> Result<(), Diagnostic> {
    for (name, line) in &doc.sections {
        if SCHEMA.iter().any(|f| f.section == *name) {
            continue;
        }
        let mut sections: Vec<&str> = SCHEMA.iter().map(|f| f.section).collect();
        sections.dedup();
        let best = sections.iter().min_by_key(|s| edit_distance(name, s)).copied().unwrap_or("node");
        let d = Diagnostic {
            path: path.to_string(),
            line: *line,
            col: 1,
            span: name.chars().count() + 2,
            message: format!("unknown section [{name}]"),
            line_text: format!("[{name}]"),
            help: if edit_distance(name, best) <= 3 {
                Some(format!("did you mean [{best}]?"))
            } else {
                Some(format!("the sections are: {}", section_list()))
            },
            note: Some("`plaine-noded --print-config` lists every section and key.".into()),
        };
        return Err(d);
    }
    for e in &doc.entries {
        if field(&e.section, &e.key).is_some() {
            continue;
        }
        let candidates: Vec<&str> =
            SCHEMA.iter().filter(|f| f.section == e.section).map(|f| f.key).collect();
        let best = candidates.iter().min_by_key(|c| edit_distance(&e.key, c)).copied();
        let help = match best {
            Some(b) if edit_distance(&e.key, b) <= 3 => format!("did you mean `{b}`?"),
            _ => format!("keys in [{}] are: {}", e.section, candidates.join(", ")),
        };
        return Err(Diagnostic {
            path: path.to_string(),
            line: e.line,
            col: e.col,
            span: e.key.chars().count(),
            message: format!("unknown key `{}` in [{}]", e.key, e.section),
            line_text: format!("{} = ...", e.key),
            help: Some(help),
            note: Some(
                "an unknown key stops startup instead of warning: a setting you believe is in \
                 force but is not is worse than a node that refuses to start."
                    .into(),
            ),
        });
    }
    Ok(())
}

fn section_list() -> String {
    let mut out: Vec<&str> = Vec::new();
    for f in SCHEMA {
        if !out.contains(&f.section) {
            out.push(f.section);
        }
    }
    out.iter().map(|s| format!("[{s}]")).collect::<Vec<_>>().join(", ")
}

fn check_types(path: &str, doc: &Document) -> Result<(), Diagnostic> {
    for e in &doc.entries {
        let Some(f) = field(&e.section, &e.key) else { continue };
        let ok = matches!(
            (f.kind, &e.value),
            (Kind::Str, Value::Str(_))
                | (Kind::Int, Value::Int(_))
                | (Kind::Bool, Value::Bool(_))
                | (Kind::StrArray, Value::Arr(_))
                | (Kind::IntArray, Value::Arr(_))
        );
        if !ok {
            return Err(diag_at(
                path,
                e,
                format!("`{}` takes {}, but this is {}", e.key, f.kind.name(), e.value.type_name()),
            ));
        }
        if f.kind == Kind::StrArray {
            if let Value::Arr(items) = &e.value {
                if let Some(bad) = items.iter().find(|v| !matches!(v, Value::Str(_))) {
                    return Err(diag_at(
                        path,
                        e,
                        format!("every element of `{}` must be a string, found {}", e.key, bad.type_name()),
                    ));
                }
            }
        }
        if f.kind == Kind::IntArray {
            if let Value::Arr(items) = &e.value {
                if let Some(bad) = items.iter().find(|v| !matches!(v, Value::Int(_))) {
                    return Err(diag_at(
                        path,
                        e,
                        format!("every element of `{}` must be an integer, found {}", e.key, bad.type_name()),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn diag_at(path: &str, e: &toml::Entry, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        path: path.to_string(),
        line: e.line,
        col: e.col,
        span: e.key.chars().count(),
        message: message.into(),
        line_text: format!("{} = ...", e.key),
        help: None,
        note: None,
    }
}

struct Reader<'a> {
    path: &'a str,
    doc: &'a Document,
    provenance: Vec<(String, String, Source)>,
}

impl<'a> Reader<'a> {
    fn note(&mut self, section: &str, key: &str, source: Source) {
        self.provenance.push((section.to_string(), key.to_string(), source));
    }

    fn str(&mut self, section: &str, key: &str, default: &str) -> String {
        match self.doc.get(section, key) {
            Some(toml::Entry { value: Value::Str(s), .. }) => {
                self.note(section, key, Source::File);
                s.clone()
            }
            _ => {
                self.note(section, key, Source::Default);
                default.to_string()
            }
        }
    }

    fn int(&mut self, section: &str, key: &str, default: i64) -> i64 {
        match self.doc.get(section, key) {
            Some(toml::Entry { value: Value::Int(i), .. }) => {
                self.note(section, key, Source::File);
                *i
            }
            _ => {
                self.note(section, key, Source::Default);
                default
            }
        }
    }

    fn bool(&mut self, section: &str, key: &str, default: bool) -> bool {
        match self.doc.get(section, key) {
            Some(toml::Entry { value: Value::Bool(b), .. }) => {
                self.note(section, key, Source::File);
                *b
            }
            _ => {
                self.note(section, key, Source::Default);
                default
            }
        }
    }

    fn strings(&mut self, section: &str, key: &str) -> Option<Vec<String>> {
        match self.doc.get(section, key) {
            Some(toml::Entry { value: Value::Arr(items), .. }) => {
                self.note(section, key, Source::File);
                Some(
                    items
                        .iter()
                        .filter_map(|v| match v {
                            Value::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect(),
                )
            }
            _ => {
                self.note(section, key, Source::Default);
                None
            }
        }
    }

    fn ints(&mut self, section: &str, key: &str) -> Option<Vec<i64>> {
        match self.doc.get(section, key) {
            Some(toml::Entry { value: Value::Arr(items), .. }) => {
                self.note(section, key, Source::File);
                Some(
                    items
                        .iter()
                        .filter_map(|v| match v {
                            Value::Int(i) => Some(*i),
                            _ => None,
                        })
                        .collect(),
                )
            }
            _ => {
                self.note(section, key, Source::Default);
                None
            }
        }
    }

    fn at(&self, section: &str, key: &str, message: impl Into<String>) -> Diagnostic {
        match self.doc.get(section, key) {
            Some(e) => diag_at(self.path, e, message),
            None => Diagnostic::general(self.path, message),
        }
    }
}

fn build(path: &str, doc: &Document, ov: &Overrides) -> Result<Config, Diagnostic> {
    let mut r = Reader { path, doc, provenance: Vec::new() };
    let mut warnings: Vec<String> = Vec::new();

    let network_text = r.str("node", "network", "main");
    let mut network = match network_text.as_str() {
        "main" => Network::Main,
        other => {
            return Err(r
                .at("node", "network", format!("`{other}` is not a network"))
                .with_help("the only network is \"main\""))
        }
    };
    if let Some(n) = ov.network {
        network = n;
        set_source(&mut r.provenance, "node", "network", Source::Cli);
    }

    let data_dir_text = r.str("node", "data_dir", "");
    let mut data_dir = if data_dir_text.is_empty() {
        crate::paths::default_data_dir()
    } else {
        expand_tilde(&data_dir_text)
    };
    if let Some(d) = &ov.data_dir {
        data_dir = d.clone();
        set_source(&mut r.provenance, "node", "data_dir", Source::Cli);
    }

    let prune = r.bool("node", "prune", true);
    let txindex = r.bool("node", "txindex", false);
    let addrindex = r.bool("node", "addrindex", false);
    let verify_frames = r.bool("node", "verify_frames", false);
    if addrindex && prune {
        warnings.push(
            "node.addrindex = true with node.prune = true: account_getHistory can only describe \
             transactions whose block bodies are still stored (the last 525960). Set \
             prune = false for a full history."
                .into(),
        );
    }
    if txindex && prune {
        warnings.push(
            "node.txindex = true with node.prune = true: the index only covers blocks whose \
             bodies are still stored (the last 525960). Set prune = false for a full history."
                .into(),
        );
    }

    let p2p_default = format!("0.0.0.0:{}", k::PORT_P2P);
    let p2p_listen = socket(&mut r, "p2p", "listen", &p2p_default)?;
    let max_peers = int_in_range(&mut r, "p2p", "max_peers", k::MAX_PEERS as i64, 1, k::MAX_PEERS as i64)?
        as usize;

    let embedded_seeds = match network {
        Network::Main => &embedded::SEEDS_MAIN,
    };
    let use_embedded_seeds = r.bool("p2p", "use_embedded_seeds", true);
    let accept_local_addrs = r.bool("p2p", "accept_local_addrs", false);
    // an explicit non-empty list wins. absent or empty falls back to the embedded
    // table, unless the operator turned that fallback off.
    let seeds = match r.strings("p2p", "seeds") {
        Some(list) if !list.is_empty() => list,
        _ if use_embedded_seeds => embedded_seeds.hosts.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    };
    if !use_embedded_seeds && seeds.is_empty() {
        warnings.push(
            "p2p.use_embedded_seeds = false with no p2p.seeds: this node will not dial anybody \
             and can only be found by peers that dial in. That is valid for a private network \
             and broken for a public one."
                .into(),
        );
    }

    let seed_port = k::PORT_P2P;
    if seeds
        .iter()
        .any(|s| matches!(crate::seeds::parse(s, seed_port), crate::seeds::Seed::Placeholder(_)))
    {
        warnings.push(format!(
            "a seed in force is a BUILD PLACEHOLDER ({}); `.invalid` is reserved by RFC 6761 \
             and can never resolve, so it can never produce a peer. The release gate refuses \
             to compile an optimised binary carrying the embedded ones at all, so this is \
             either a development build or a placeholder written into p2p.seeds by hand. Set \
             p2p.seeds to names that exist.",

            seeds
                .iter()
                .find(|s| {
                    matches!(crate::seeds::parse(s, seed_port), crate::seeds::Seed::Placeholder(_))
                })
                .cloned()
                .unwrap_or_default()
        ));
    }

    let stratum_default = format!("0.0.0.0:{}", k::PORT_STRATUM);
    let stratum_listen = socket(&mut r, "stratum", "listen", &stratum_default)?;
    let stratum_max_connections = int_in_range(
        &mut r,
        "stratum",
        "max_connections",
        256,
        1,
        STRATUM_CONNECTION_CEILING as i64,
    )? as usize;
    if stratum_max_connections > 1024 {
        warnings.push(format!(
            "stratum.max_connections = {stratum_max_connections}: raise the service's \
             LimitNOFILE to {} as well, and note that vardiff lengthens its setpoint floor to \
             keep share checking inside the CPU budget.",
            k::LIMIT_NOFILE
        ));
    }

    let stratum_enforcement = r.bool("stratum", "enforcement", true);
    let table_ceiling: i64 = 16_777_216;

    let mut stratum_max_per_ip =
        int_in_range(&mut r, "stratum", "max_per_ip", Caps::SOLO.max_per_ip as i64, 0, i64::MAX)? as usize;
    let mut stratum_new_conns_per_ip_per_min = int_in_range(
        &mut r,
        "stratum",
        "new_conns_per_ip_per_min",
        Caps::SOLO.new_conns_per_ip_per_min as i64,
        0,
        u32::MAX as i64,
    )? as u32;
    let mut stratum_global_accept_per_sec = int_in_range(
        &mut r,
        "stratum",
        "global_accept_per_sec",
        Caps::SOLO.global_accept_per_sec as i64,
        0,
        u32::MAX as i64,
    )? as u32;

    let mut stratum_bans = BanPolicy {
        enabled: r.bool("stratum", "bans_enabled", BanPolicy::DEFAULT.enabled),
        threshold: int_in_range(&mut r, "stratum", "ban_threshold", BanPolicy::DEFAULT.threshold as i64, 0, u32::MAX as i64)? as u32,
        soft_cap: int_in_range(&mut r, "stratum", "ban_soft_cap", BanPolicy::DEFAULT.soft_cap as i64, 0, u32::MAX as i64)? as u32,
        decay_secs: int_in_range(&mut r, "stratum", "ban_decay_secs", BanPolicy::DEFAULT.decay_secs as i64, 1, i64::MAX)? as u64,
        base_secs: int_in_range(&mut r, "stratum", "ban_base_secs", BanPolicy::DEFAULT.base_secs as i64, 1, i64::MAX)? as u64,
        ladder_factor: int_in_range(&mut r, "stratum", "ban_ladder_factor", BanPolicy::DEFAULT.ladder_factor as i64, 1, i64::MAX)? as u64,
        max_secs: int_in_range(&mut r, "stratum", "ban_max_secs", BanPolicy::DEFAULT.max_secs as i64, 1, i64::MAX)? as u64,
        table_entries: int_in_range(&mut r, "stratum", "ban_table_entries", BanPolicy::DEFAULT.table_entries as i64, 1, table_ceiling)? as usize,
        throttle_ms: int_in_range(&mut r, "stratum", "garbage_throttle_secs", (BanPolicy::DEFAULT.throttle_ms / 1000) as i64, 0, i64::MAX)? as u64 * 1000,
    };

    let submit_per_sec = int_in_range(&mut r, "stratum", "submit_rate_per_sec", RatePolicy::DEFAULT.submit_per_sec as i64, 0, i64::MAX)? as f64;
    let submit_burst = int_in_range(&mut r, "stratum", "submit_burst", RatePolicy::DEFAULT.submit_burst as i64, 0, i64::MAX)? as f64;
    let line_per_sec = int_in_range(&mut r, "stratum", "line_rate_per_sec", RatePolicy::DEFAULT.line_per_sec as i64, 0, i64::MAX)? as f64;
    let line_burst = int_in_range(&mut r, "stratum", "line_burst", RatePolicy::DEFAULT.line_burst as i64, 0, i64::MAX)? as f64;
    let mut stratum_rates = RatePolicy {
        submit_enabled: submit_per_sec > 0.0,
        submit_per_sec,
        submit_burst,
        line_enabled: line_per_sec > 0.0,
        line_per_sec,
        line_burst,
    };

    let mut stratum_auth_deadline_ms =
        int_in_range(&mut r, "stratum", "auth_deadline_secs", 10, 0, i64::MAX)? as u64 * 1000;
    let mut stratum_idle_evict_ms =
        int_in_range(&mut r, "stratum", "idle_evict_secs", 1800, 0, i64::MAX)? as u64 * 1000;
    let mut stratum_read_deadline_ms =
        int_in_range(&mut r, "stratum", "read_deadline_secs", 600, 0, i64::MAX)? as u64 * 1000;
    let mut stratum_write_timeout_ms =
        int_in_range(&mut r, "stratum", "write_timeout_secs", 30, 0, i64::MAX)? as u64 * 1000;
    let mut stratum_out_buf_cap =
        int_in_range(&mut r, "stratum", "out_buf_cap_bytes", 4 * 1024, 0, i64::MAX)? as usize;

    let stratum_setpoint_secs =
        int_in_range(&mut r, "stratum", "vardiff_setpoint_secs", 0, 0, i64::MAX)? as u64;
    let stratum_tick_ms =
        int_in_range(&mut r, "stratum", "vardiff_tick_secs", 2, 1, i64::MAX)? as u64 * 1000;
    let vardiff_min_diff =
        int_in_range(&mut r, "stratum", "vardiff_min_diff", DiffPolicy::DEFAULT.min_diff as i64, 1, i64::MAX)? as u64;
    let vardiff_start_diff =
        int_in_range(&mut r, "stratum", "vardiff_start_diff", DiffPolicy::DEFAULT.start_diff as i64, 1, i64::MAX)? as u64;

    let (ladder, ladder_len) = match r.ints("stratum", "vardiff_ladder") {
        Some(list) => {
            if list.is_empty() || list.len() > plaine_stratum::limits::LADDER_MAX {
                return Err(r
                    .at("stratum", "vardiff_ladder", format!(
                        "vardiff_ladder must have 1..={} entries", plaine_stratum::limits::LADDER_MAX
                    ))
                    .with_help("each entry is a mantissa x10 in 10..=999, e.g. [10, 12, 15, 20, 40]"));
            }
            let mut arr = [0f64; plaine_stratum::limits::LADDER_MAX];
            let mut prev = 0i64;
            for (i, v) in list.iter().enumerate() {
                if *v < 10 || *v > 999 {
                    return Err(r
                        .at("stratum", "vardiff_ladder", format!(
                            "vardiff_ladder entry {v} is outside 10..=999"
                        ))
                        .with_help("entries are mantissas x10: 10 = 1.0, 15 = 1.5, 80 = 8.0"));
                }
                if *v <= prev {
                    return Err(r
                        .at("stratum", "vardiff_ladder", format!(
                            "vardiff_ladder must strictly increase; {v} follows {prev}"
                        ))
                        .with_help("list the rungs low to high with no repeats"));
                }
                prev = *v;
                arr[i] = *v as f64 / 10.0;
            }
            (arr, list.len())
        }
        None => (Cadence::DEFAULT.ladder, Cadence::DEFAULT.ladder_len),
    };

    let stratum_cadence = Cadence {
        warmup_shares: int_in_range(&mut r, "stratum", "vardiff_warmup_shares", Cadence::DEFAULT.warmup_shares as i64, 0, u32::MAX as i64)? as u32,
        warmup_gate_shares: int_in_range(&mut r, "stratum", "vardiff_warmup_gate_shares", Cadence::DEFAULT.warmup_gate_shares as i64, 1, u32::MAX as i64)? as u32,
        retarget_gate_shares: int_in_range(&mut r, "stratum", "vardiff_retarget_gate_shares", Cadence::DEFAULT.retarget_gate_shares as i64, 1, u32::MAX as i64)? as u32,
        retarget_gate_secs: int_in_range(&mut r, "stratum", "vardiff_retarget_gate_secs", Cadence::DEFAULT.retarget_gate_secs as i64, 1, i64::MAX)? as f64,
        mature_shares: int_in_range(&mut r, "stratum", "vardiff_mature_shares", Cadence::DEFAULT.mature_shares as i64, 0, u32::MAX as i64)? as u32,
        max_step: int_in_range(&mut r, "stratum", "vardiff_max_step", Cadence::DEFAULT.max_step as i64, 2, 1_000_000)? as f64,
        dead_zone: int_in_range(&mut r, "stratum", "vardiff_dead_zone_pct", (Cadence::DEFAULT.dead_zone * 100.0) as i64, 101, 100_000)? as f64 / 100.0,
        mature_zone: int_in_range(&mut r, "stratum", "vardiff_mature_zone_pct", (Cadence::DEFAULT.mature_zone * 100.0) as i64, 101, 100_000)? as f64 / 100.0,
        fast_escape: int_in_range(&mut r, "stratum", "vardiff_fast_escape_pct", (Cadence::DEFAULT.fast_escape * 100.0) as i64, 101, 100_000)? as f64 / 100.0,
        silence_slack: int_in_range(&mut r, "stratum", "vardiff_silence_slack", Cadence::DEFAULT.silence_slack as i64, 1, i64::MAX)? as f64,
        tau: [
            int_in_range(&mut r, "stratum", "vardiff_dsps_tau_fast_secs", Cadence::DEFAULT.tau[0] as i64, 1, i64::MAX)? as f64,
            int_in_range(&mut r, "stratum", "vardiff_dsps_tau_mid_secs", Cadence::DEFAULT.tau[1] as i64, 1, i64::MAX)? as f64,
            int_in_range(&mut r, "stratum", "vardiff_dsps_tau_slow_secs", Cadence::DEFAULT.tau[2] as i64, 1, i64::MAX)? as f64,
        ],
        ladder,
        ladder_len,
    };

    let stratum_vardiff_enabled = r.bool("stratum", "vardiff_enabled", true);
    let stratum_vardiff_fixed_diff =
        int_in_range(&mut r, "stratum", "vardiff_fixed_diff", 0, 0, i64::MAX)? as u64;
    let stratum_vardiff_max_diff =
        int_in_range(&mut r, "stratum", "vardiff_max_diff", 0, 0, i64::MAX)? as u64;
    if stratum_vardiff_max_diff != 0 && stratum_vardiff_max_diff < vardiff_min_diff {
        return Err(r
            .at("stratum", "vardiff_max_diff", format!(
                "vardiff_max_diff = {stratum_vardiff_max_diff} is below vardiff_min_diff = {vardiff_min_diff}"
            ))
            .with_help("0 = cap at network difficulty; any other value must be at or above the floor"));
    }
    if stratum_vardiff_enabled && stratum_vardiff_fixed_diff != 0 {
        warnings.push(
            "stratum.vardiff_fixed_diff is set but stratum.vardiff_enabled is true, so it is \
             ignored. Set vardiff_enabled = false to run at a fixed difficulty."
                .into(),
        );
    }

    let mut stratum_diff = DiffPolicy {
        start_diff: vardiff_start_diff,
        min_diff: vardiff_min_diff,
        cache_entries: int_in_range(&mut r, "stratum", "diff_cache_entries", DiffPolicy::DEFAULT.cache_entries as i64, 1, table_ceiling)? as usize,
        cache_ttl_ms: int_in_range(&mut r, "stratum", "diff_cache_ttl_secs", (DiffPolicy::DEFAULT.cache_ttl_ms / 1000) as i64, 1, i64::MAX)? as u64 * 1000,
        storm_enabled: true,
        storm_per_min: int_in_range(&mut r, "stratum", "reconnect_storm_per_min", DiffPolicy::DEFAULT.storm_per_min as i64, 0, u32::MAX as i64)? as u32,
        storm_window_ms: int_in_range(&mut r, "stratum", "reconnect_storm_window_secs", (DiffPolicy::DEFAULT.storm_window_ms / 1000) as i64, 1, i64::MAX)? as u64 * 1000,
    };
    stratum_diff.storm_enabled = stratum_diff.storm_per_min > 0;

    if !stratum_enforcement {
        stratum_bans.enabled = false;
        stratum_bans.throttle_ms = 0;
        stratum_rates.submit_enabled = false;
        stratum_rates.line_enabled = false;
        stratum_diff.storm_enabled = false;
        stratum_max_per_ip = 0;
        stratum_new_conns_per_ip_per_min = 0;
        stratum_global_accept_per_sec = 0;
        stratum_auth_deadline_ms = 0;
        stratum_idle_evict_ms = 0;
        stratum_read_deadline_ms = 0;
        stratum_write_timeout_ms = 0;
        stratum_out_buf_cap = 0;
        warnings.push(
            "stratum.enforcement = false: the node applies no bans, rate limits, connection \
             caps or idle timeouts of its own. Difficulty adjustment still runs. Drive policy \
             from an external system (fail2ban, a pool front end) or the port is open to abuse."
                .into(),
        );
    }
    if stratum_diff.min_diff > stratum_diff.start_diff {
        return Err(r
            .at("stratum", "vardiff_start_diff", format!(
                "vardiff_start_diff = {} is below vardiff_min_diff = {}",
                stratum_diff.start_diff, stratum_diff.min_diff
            ))
            .with_help("the starting difficulty cannot be under the floor"));
    }

    let rpc_default = format!("127.0.0.1:{}", k::PORT_RPC);
    let rpc_listen = socket(&mut r, "rpc", "listen", &rpc_default)?;
    let token_text = r.str("rpc", "token", "");
    let rpc_token = if token_text.is_empty() { None } else { Some(token_text) };

    let probe = plaine_rpc::RpcConfig {
        bind: rpc_listen,
        token: rpc_token.clone(),
        ..plaine_rpc::RpcConfig::loopback(rpc_listen.port())
    };
    if let Err(e) = plaine_rpc::check_bind_policy(&probe) {
        return Err(r
            .at("rpc", "listen", "the RPC listener is not safe as configured")
            .with_help(e.to_string()));
    }
    if !plaine_rpc::server::is_loopback(&rpc_listen) {
        warnings.push(format!(
            "rpc.listen = {rpc_listen} is reachable from the network. The token is required and \
             present, but there is no TLS in the node: put a reverse proxy in \
             front of it or the token crosses the wire in clear text."
        ));
    }

    // Default sits at the consensus floor (1 mile = 0.000001 PLNE), the way a
    // bitcoin node ships a tiny minrelay. The fee market does the rest: the
    // mempool orders and evicts by fee-per-byte, so congestion raises the real
    // floor on its own. Raise this only to shed dust when a node is under load.
    let relay_fee_mile =
        int_in_range(&mut r, "mempool", "relay_fee_mile", 1, 1, i64::MAX)? as u128;
    let mempool_max_txs = int_in_range(
        &mut r,
        "mempool",
        "max_txs",
        k::MAX_MEMPOOL_TXS as i64,
        1,
        k::MAX_MEMPOOL_TXS as i64,
    )? as usize;

    let author_note = r.str("mining", "author_note", "").into_bytes();
    if author_note.len() > k::AUTHOR_NOTE_MAX_BYTES {
        return Err(r
            .at(
                "mining",
                "author_note",
                format!(
                    "author_note is {} bytes; the consensus limit is {}",
                    author_note.len(),
                    k::AUTHOR_NOTE_MAX_BYTES
                ),
            )
            .with_help("SPEC 8 caps the coinbase note at 256 bytes. Shorten it.")
            .with_note("the limit is on bytes, not characters: non-ASCII text costs 2-4 each."));
    }
    if !author_note.is_empty() {
        warnings.push(
            "mining.author_note is set. SPEC 8: a constant string links every block you ever \
             mine to every other one. The default is empty for that reason."
                .into(),
        );
    }

    let enabled = r.bool("checkpoints", "enabled", true);
    let key_texts = r.strings("checkpoints", "keys").unwrap_or_default();

    if key_texts.iter().any(|s| s.trim().is_empty()) {
        return Err(r
            .at("checkpoints", "keys", "`checkpoints.keys` contains an empty string")
            .with_help(
                "an empty entry is not a way to switch the layer off, and it is not a key \
                 either. Say which you meant:\n\n  \
                 - use the official embedded key (the normal case):  keys = []   (or delete the \
                 line)\n  \
                 - switch the checkpoint layer off completely:       enabled = false\n  \
                 - use your own key(s):                              keys = [\"<64 hex chars>\"]",
            )
            .with_note(
                "no value of `keys` can disable the layer. Emptying a list is what people do to \
                 turn something off, so here it lands on the embedded key instead.",
            ));
    }
    if !enabled && !key_texts.is_empty() {
        return Err(r
            .at(
                "checkpoints",
                "enabled",
                "`checkpoints.enabled = false` with keys listed says two opposite things",
            )
            .with_help(
                "delete the `keys` line to turn the layer off, or set `enabled = true` to use \
                 the keys you listed.",
            ));
    }

    let mut keys: Vec<[u8; 32]> = Vec::new();
    for (i, text) in key_texts.iter().enumerate() {
        let t = text.strip_prefix("0x").unwrap_or(text);
        if t.chars().count() != 64 {
            return Err(r
                .at(
                    "checkpoints",
                    "keys",
                    format!(
                        "checkpoints.keys[{i}] is {} characters; an ed25519 public key is 64 hex \
                         characters",
                        t.chars().count()
                    ),
                )
                .with_help("this is the public half of the authority key, published by whoever \
                            operates it. There is no private key anywhere in a node."));
        }
        let bytes = plaine_consensus::hex::decode(t).map_err(|_| {
            r.at(
                "checkpoints",
                "keys",
                format!("checkpoints.keys[{i}] is not valid hexadecimal"),
            )
        })?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        if !embedded::is_valid_pubkey(&key) {
            return Err(r
                .at(
                    "checkpoints",
                    "keys",
                    format!("checkpoints.keys[{i}] is not a valid ed25519 public key"),
                )
                .with_help(
                    "the 64 characters decoded fine but are not a point on the curve - a \
                     transcription error.",
                )
                .with_note(
                    "checked at startup: a wrong key would otherwise fail to verify every \
                     checkpoint silently, and you would believe you were protected.",
                ));
        }
        if keys.contains(&key) {
            return Err(r.at(
                "checkpoints",
                "keys",
                format!("checkpoints.keys[{i}] is listed twice"),
            ));
        }
        keys.push(key);
    }

    // Zero-config default: layer enabled, no keys given -> use the key compiled into
    // this binary. An operator only ever overrides one, never types one in.
    let checkpoints = if !enabled {
        CheckpointPolicy::Disabled
    } else if keys.is_empty() {
        CheckpointPolicy::Embedded {
            key: embedded::CHECKPOINT_AUTHORITY_KEY.bytes,
            placeholder: embedded::CHECKPOINT_AUTHORITY_KEY.placeholder,
        }
    } else {
        CheckpointPolicy::Configured { keys }
    };

    let threshold = int_in_range(&mut r, "checkpoints", "threshold", 1, 1, i64::MAX)? as usize;
    let in_force = checkpoints.keys().len();
    if checkpoints.is_enabled() && threshold > in_force {
        return Err(r
            .at(
                "checkpoints",
                "threshold",
                format!("threshold is {threshold} but only {in_force} key(s) are configured"),
            )
            .with_help(
                "a threshold above the number of keys can never be met, so no checkpoint would \
                 ever verify and the layer would be off without saying so.",
            ));
    }
    if matches!(checkpoints, CheckpointPolicy::Disabled) {
        warnings.push(
            "Checkpoints are off (checkpoints.enabled = false). Layers 1, 2 and 4 of the 51% \
             defence still apply; the signed-checkpoint layer does not. This node will accept \
             deep reorgs that a checkpointed node rejects."
                .into(),
        );
    }

    for key in checkpoints.keys() {
        if let Some(role) = embedded::placeholder_role(&key) {
            warnings.push(format!(
                "the checkpoint key in force is the BUILD PLACEHOLDER (fp {}, {role}). Its bytes \
                 are the ASCII text `CHECKPOINT-PLACEHOLDER-NOT-REAL0` and no signature will ever \
                 verify against it, so the checkpoint layer is effectively off while it is in \
                 use. Never acceptable on the live network.",
                embedded::fingerprint(&key)
            ));
        }
    }

    // same zero-config shape as the checkpoint key: empty means the embedded author
    // key. A value set here has to decode to a real curve point, or startup fails.
    let author_text = r.str("author", "pubkey", "");
    let (author_pubkey, author_key_source, author_key_placeholder) = if author_text.trim().is_empty()
    {
        (
            embedded::AUTHOR_KEY.bytes,
            plaine_rpc::views::KeySource::Embedded,
            embedded::AUTHOR_KEY.placeholder,
        )
    } else {
        let t = author_text.strip_prefix("0x").unwrap_or(&author_text);
        if t.chars().count() != 64 {
            return Err(r
                .at(
                    "author",
                    "pubkey",
                    format!(
                        "author.pubkey is {} characters; an ed25519 public key is 64 hex \
                         characters",
                        t.chars().count()
                    ),
                )
                .with_help("leave it empty to use the key embedded in this binary."));
        }
        let bytes = plaine_consensus::hex::decode(t)
            .map_err(|_| r.at("author", "pubkey", "author.pubkey is not valid hexadecimal"))?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        if !embedded::is_valid_pubkey(&key) {
            return Err(r
                .at("author", "pubkey", "author.pubkey is not a valid ed25519 public key")
                .with_help(
                    "the 64 characters decoded fine but are not a point on the curve - a \
                     transcription error.",
                )
                .with_note(
                    "a wrong author key does not endanger consensus, but it would make every genuine announcement invisible to you.",
                ));
        }
        (key, plaine_rpc::views::KeySource::Config, false)
    };

    let _ = author_key_placeholder;
    if embedded::placeholder_role(&author_pubkey).is_some() {
        warnings.push(format!(
            "the author announcement key in force is the BUILD PLACEHOLDER (fp {}). Its bytes are \
             the ASCII text `AUTHOR-KEY-PLACEHOLDER-NOT-REAL2`; nothing can sign for it, so no \
             announcement will ever appear. SPEC 4.1 rule 1 also makes this key a block-validity \
             input - do not point a node at mainnet with it.",
            embedded::fingerprint(&author_pubkey)
        ));
    }
    let author_show_in_log = r.bool("author", "show_in_log", true);
    if !author_show_in_log {
        warnings.push(
            "author.show_in_log = false: emergency announcements from the author will not be \
             printed here. They remain readable with the author_getNotes RPC."
                .into(),
        );
    }

    let level_text = r.str("log", "level", "info");
    let mut log_level = LogLevel::parse(&level_text).ok_or_else(|| {
        r.at("log", "level", format!("`{level_text}` is not a log level"))
            .with_help("levels are: error, warn, info, debug, trace")
    })?;
    if let Some(l) = ov.log_level {
        log_level = l;
        set_source(&mut r.provenance, "log", "level", Source::Cli);
    }

    Ok(Config {
        network,
        data_dir,
        prune,
        txindex,
        addrindex,
        verify_frames,
        p2p_listen,
        max_peers,
        seeds,
        use_embedded_seeds,
        accept_local_addrs,
        stratum_listen,
        stratum_max_connections,
        stratum_enforcement,
        stratum_max_per_ip,
        stratum_new_conns_per_ip_per_min,
        stratum_global_accept_per_sec,
        stratum_bans,
        stratum_rates,
        stratum_diff,
        stratum_cadence,
        stratum_vardiff_enabled,
        stratum_vardiff_fixed_diff,
        stratum_vardiff_max_diff,
        stratum_setpoint_secs,
        stratum_auth_deadline_ms,
        stratum_idle_evict_ms,
        stratum_read_deadline_ms,
        stratum_write_timeout_ms,
        stratum_out_buf_cap,
        stratum_tick_ms,
        rpc_listen,
        rpc_token,
        relay_fee_mile,
        mempool_max_txs,
        author_note,
        checkpoints,
        checkpoint_threshold: threshold,
        author_pubkey,
        author_key_source,
        author_key_placeholder,
        author_show_in_log,
        log_level,
        provenance: r.provenance,
        warnings,
    })
}

fn set_source(prov: &mut [(String, String, Source)], section: &str, key: &str, s: Source) {
    if let Some(e) = prov.iter_mut().find(|(sec, k, _)| sec == section && k == key) {
        e.2 = s;
    }
}

fn socket(r: &mut Reader<'_>, section: &str, key: &str, default: &str) -> Result<SocketAddr, Diagnostic> {
    let text = r.str(section, key, default);
    text.parse::<SocketAddr>().map_err(|_| {
        let help = if text.contains(':') {
            "an address is `host:port`, for example \"127.0.0.1:9257\" or \"[::1]:9257\". A \
             hostname is not accepted here: this is a bind address, not a destination."
        } else {
            "the port is missing. Write it as `address:port`, for example \"0.0.0.0:9256\"."
        };
        r.at(section, key, format!("`{text}` is not an address to listen on")).with_help(help)
    })
}

fn int_in_range(
    r: &mut Reader<'_>,
    section: &str,
    key: &str,
    default: i64,
    lo: i64,
    hi: i64,
) -> Result<i64, Diagnostic> {
    let v = r.int(section, key, default);
    if v < lo || v > hi {
        let extra = match (section, key) {
            ("stratum", "max_connections") => Some(format!(
                "the ceiling of {hi} comes with the file-descriptor budget; raise \
                 LimitNOFILE to {} before going near it.",
                k::LIMIT_NOFILE
            )),
            ("mempool", "relay_fee_mile") => Some(
                "this is the operator's RELAY policy. The consensus floor is 1 mile and config \
                 cannot move it."
                    .into(),
            ),
            ("p2p", "max_peers") | ("mempool", "max_txs") => {
                Some("this ceiling is a frozen consensus-adjacent limit.".into())
            }
            _ => None,
        };
        let d = r
            .at(section, key, format!("{section}.{key} = {v} is outside {lo}..={hi}"))
            .with_help(format!("the default is {default}"));
        return Err(match extra {
            Some(n) => d.with_note(n),
            None => d,
        });
    }
    Ok(v)
}

pub fn default_config_text(network: Network) -> String {
    format!(
        r##"# noded.toml - Plaine node configuration
#
# Every setting here is already the default, and every line is commented out.
# This file changes nothing as written. It exists so you can see what exists
# without reading documentation. Delete it and restart to get it back.
#
# Uncomment a line to change it. An empty file is valid and is the normal case.

[node]
# network = "{network}"        # the only network is "main"
# data_dir = "{data_dir}"
                               # Windows default: %APPDATA%\Plaine
                               # Linux/macOS default: ~/.plaine
                               # Use 'single quotes' for a Windows path so the
                               # backslashes are not read as escapes.
# prune = true                 # keep the last {year} block bodies (~1 year).
                               # false = archive node, keeps everything.
# txindex = false              # index every txid so tx_get can find confirmed
                               # transactions. Costs ~0.9 GB/year and requires
                               # a resync to build.
# addrindex = false            # index which transactions touch each address, so
                               # account_getHistory can list them (the desktop
                               # wallet needs it). Covers blocks connected after
                               # it is switched on; resync for the full chain.
# verify_frames = false        # the two deep integrity sweeps at boot.
                               # L2 (bodies): re-checks every frame of every
                               # sealed body segment against its anchor. Catches
                               # an interior same-length payload substitution
                               # with its CRC fixed up, which is the one class
                               # the always-on L1 check cannot see. It says
                               # nothing about the header stream.
                               # L3-H (headers): walks the chain link inside
                               # each sealed header segment. Catches interior
                               # header bit rot - up to 4094 headers per segment
                               # that L1 and L2 both miss and the node serves.
                               # Slow - L2 is 4096 scattered reads per segment -
                               # so run it after a disk event, not every start.

[p2p]
# listen = "0.0.0.0:{p2p}"
# max_peers = {peers}                # frozen ceiling: {peers}
# seeds = []                   # empty = use the seeds built into this binary,
                               # which are DNS names, not addresses, so the
                               # network can move without a new release.
                               # List your own only for a private network.
# use_embedded_seeds = true    # false = an empty `seeds` really means none.
                               # The node then dials nobody and can only be
                               # found by peers that dial in.

[stratum]
# listen = "0.0.0.0:{stratum}"      # the built-in solo stratum server
# max_connections = 256        # ceiling {ceiling}; raise LimitNOFILE with it
#
# Every limit below is settable. A limit of 0 means "no limit" for that line
# (rates, caps, timeouts, throttle), and every timeout at 0 never fires.
# enforcement = true           # false turns off all built-in policy at once:
                               # no bans, rate limits, caps or timeouts. The
                               # node stops policing itself and you drive policy
                               # from outside (fail2ban, a pool front end).
                               # Difficulty adjustment keeps running either way.
#
# connection caps
# max_per_ip = 16              # 0 = unlimited concurrent conns per address
# new_conns_per_ip_per_min = 6 # 0 = no per-address connect rate limit
# global_accept_per_sec = 50   # 0 = no global accept rate limit
#
# bans and misbehavior scoring
# bans_enabled = true          # false disables scoring, bans and throttling
# ban_threshold = 100          # score that triggers a ban; 0 = never ban
# ban_soft_cap = 25            # ceiling on the soft (stale) part of a score
# ban_decay_secs = 6           # one score point forgiven every this many secs
# ban_base_secs = 600          # first ban length
# ban_ladder_factor = 4        # each repeat ban multiplies the length by this
# ban_max_secs = 86400         # ban length ceiling
# ban_table_entries = 65536    # tracked addresses (LRU)
# garbage_throttle_secs = 60   # throttle window for a garbage source; 0 = off
#
# rate limits
# submit_rate_per_sec = 3      # share submits per second; 0 = unlimited
# submit_burst = 10            # submit burst allowance
# line_rate_per_sec = 20       # messages per second; 0 = unlimited
# line_burst = 60              # message burst allowance
#
# timeouts (0 = never disconnect on this)
# auth_deadline_secs = 10      # drop a connection that never authorizes
# idle_evict_secs = 1800       # drop an authorized connection sending no shares
# read_deadline_secs = 600     # drop a connection sending no bytes
# write_timeout_secs = 30      # drop a connection that cannot take writes
# out_buf_cap_bytes = 4096     # drop a slow reader whose out buffer passes this
#
# vardiff (difficulty control; bounds, retarget cadence and controller shape)
# vardiff_enabled = true       # false turns off node-side vardiff: every miner
                               # runs at a fixed difficulty (vardiff_fixed_diff,
                               # or vardiff_start_diff if that is 0) and the node
                               # never retargets. Hand difficulty control to a
                               # pool front end this way. The +difficulty login
                               # suffix still pins a per-miner difficulty.
# vardiff_fixed_diff = 0       # difficulty used when vardiff_enabled = false;
                               # 0 = use vardiff_start_diff
# vardiff_setpoint_secs = 0    # target secs per share; 0 = auto from the budget
# vardiff_start_diff = 60000   # starting difficulty for a fresh miner
# vardiff_min_diff = 8192      # difficulty floor
# vardiff_max_diff = 0         # difficulty ceiling; 0 = cap at network difficulty
# vardiff_tick_secs = 2        # vardiff evaluation period
# vardiff_retarget_gate_shares = 20   # shares before a normal retarget
# vardiff_retarget_gate_secs = 120    # or this long since the last retarget
# vardiff_warmup_shares = 30   # a miner is "warming up" below this share count
# vardiff_warmup_gate_shares = 5      # retarget gate while warming up
# vardiff_mature_shares = 60   # a miner is "mature" at or above this count
# vardiff_max_step = 4         # largest per-retarget difficulty step factor
# vardiff_dead_zone_pct = 150  # young miner: retarget only past 1.50x off target
# vardiff_mature_zone_pct = 115  # mature miner: tighter band, 1.15x off target
# vardiff_fast_escape_pct = 170  # skip the gate when this far off (x100)
# vardiff_silence_slack = 4    # lower difficulty after this many setpoints silent
# vardiff_dsps_tau_fast_secs = 60     # share-rate smoothing windows (three EMAs)
# vardiff_dsps_tau_mid_secs = 300
# vardiff_dsps_tau_slow_secs = 3600
# vardiff_ladder = [10, 12, 15, 20, 25, 30, 40, 50, 60, 80]
                               # difficulty rounding rungs, each a mantissa x10
                               # (10 = 1.0). Must increase, 1 to 16 entries.
#
# difficulty cache and reconnect storm floor
# diff_cache_entries = 65536
# diff_cache_ttl_secs = 86400
# reconnect_storm_per_min = 4  # 0 = do not pin a floor on reconnect storms
# reconnect_storm_window_secs = 300

[rpc]
# listen = "127.0.0.1:{rpc}"
# token = ""                   # required if `listen` is not a loopback address.
                               # On loopback no token is used: anything that can
                               # reach 127.0.0.1 is already a local user.
                               # The node refuses to start on a public address
                               # with no token. There is no TLS here - put a
                               # reverse proxy in front if you expose it.

[mempool]
# relay_fee_mile = 1            # default; this node's relay policy, not a consensus
                               # rule. The consensus floor is 1 mile and nothing in
                               # this file can move it. Raise this to shed dust.
# max_txs = {maxtx}

[mining]
# author_note = ""             # your own comment in the blocks you mine.
                               # Empty by default deliberately: a constant
                               # string links every block you ever mine to
                               # every other one. Max 256 bytes.

[checkpoints]
# ---------------------------------------------------------------------------
# Signed checkpoints are a temporary defence for the chain's first year
#. They can only reject a deep reorg; they never create, reorder or
# censor blocks. They stop working by themselves at height {sunset} - that
# height is compiled into the binary and nothing in this file can extend it.
#
# Three states, and they cannot be confused for one another:
#
#   1. use the official embedded key   <- the normal case, and the default
#      write nothing, or:  keys = []
#
#   2. use your own key(s)             <- key rotation, or a private network
#      keys = ["<64 hex characters>"]
#
#   3. turn the layer off entirely     <- you lose protection against deep reorgs
#      enabled = false
#
# Note the asymmetry, it is deliberate: no value of `keys` can ever switch the
# layer off. Emptying a list is what people do when they want something off,
# so emptying this one lands on the safe answer (the embedded key) instead.
# Switching it off takes the word `false` on the `enabled` line, which nobody
# types by accident. Whichever state is in force is printed at every startup.
# ---------------------------------------------------------------------------
# enabled = true
# threshold = 1                # frozen at 1 with exactly one key.
                               # It is a setting rather than a constant only so
                               # that a future change would be a config edit
                               # instead of a fork.
# keys = []                    # empty = the official key embedded in this
                               # binary. You never need to fill this in.

[author]
# The author announcement key. A separate key from the checkpoint
# one, and it has no sunset - because all it can do is write at most 1 KiB of
# text into a block. It cannot reject a block, reorder a transaction, censor
# anything or create a coin. That is why it is safe to leave it permanent, and
# why a leak of it costs false text and nothing else.
#
# This is how an emergency algorithm change reaches you when a website, a
# domain or a messenger has been taken away: the message is in the chain.
# `pubkey` empty means the key embedded in this binary, which is the normal
# case. There is no `enabled` here, because there is nothing to disable.
# pubkey = ""                  # empty = embedded key. Fill in only on rotation.
# show_in_log = true           # print new announcements to this log

[log]
# level = "info"               # error | warn | info | debug | trace
"##,
        network = network.as_str(),
        data_dir = crate::paths::default_data_dir().display().to_string().replace('\\', "\\\\"),
        year = k::BLOCKS_PER_YEAR,
        p2p = k::PORT_P2P,
        stratum = k::PORT_STRATUM,
        rpc = k::PORT_RPC,
        peers = k::MAX_PEERS,
        ceiling = STRATUM_CONNECTION_CEILING,
        maxtx = k::MAX_MEMPOOL_TXS,
        sunset = k::CHECKPOINT_SUNSET_HEIGHT,
    )
}

impl Config {
    pub fn print_effective(&self, paths: &Paths) -> String {
        let mut out = String::new();
        out.push_str("# effective configuration (value <- where it came from)\n\n");
        let mut current = "";
        for (section, key, source) in &self.provenance {
            if section != current {
                out.push_str(&format!("[{section}]\n"));
                current = section;
            }
            out.push_str(&format!(
                "{key} = {}  # {}\n",
                self.value_of(section, key),
                source.label()
            ));
        }
        out.push_str("\n# resolved, not settable directly:\n");
        out.push_str(&format!("checkpoints  = {}\n", self.checkpoints.describe()));
        out.push_str(&format!(
            "author key   = {} (fp {}{})\n",
            match self.author_key_source {
                plaine_rpc::views::KeySource::Embedded => "embedded",
                plaine_rpc::views::KeySource::Config => "config",
            },
            embedded::fingerprint(&self.author_pubkey),

            if embedded::placeholder_role(&self.author_pubkey).is_some() {
                ", PLACEHOLDER"
            } else {
                ""
            }
        ));
        out.push_str(&format!("config file  = {}\n", paths.config_file.display()));
        out.push_str(&format!("chain dir    = {}\n", paths.chain_dir.display()));
        out.push_str(&format!("database     = {}\n", paths.database_file.display()));
        out.push_str(&format!(
            "sunset       = height {} (compile-time; config cannot change it)\n",
            k::CHECKPOINT_SUNSET_HEIGHT
        ));
        out
    }

    fn value_of(&self, section: &str, key: &str) -> String {
        match (section, key) {
            ("node", "network") => format!("{:?}", self.network.as_str()),
            ("node", "data_dir") => format!("{:?}", self.data_dir.display().to_string()),
            ("node", "prune") => self.prune.to_string(),
            ("node", "txindex") => self.txindex.to_string(),
            ("node", "addrindex") => self.addrindex.to_string(),
            ("node", "verify_frames") => self.verify_frames.to_string(),
            ("p2p", "listen") => format!("{:?}", self.p2p_listen.to_string()),
            ("p2p", "max_peers") => self.max_peers.to_string(),
            ("p2p", "seeds") => format!("{:?}", self.seeds),
            ("p2p", "use_embedded_seeds") => self.use_embedded_seeds.to_string(),
            ("stratum", "listen") => format!("{:?}", self.stratum_listen.to_string()),
            ("stratum", "max_connections") => self.stratum_max_connections.to_string(),
            ("stratum", "enforcement") => self.stratum_enforcement.to_string(),
            ("stratum", "max_per_ip") => self.stratum_max_per_ip.to_string(),
            ("stratum", "new_conns_per_ip_per_min") => self.stratum_new_conns_per_ip_per_min.to_string(),
            ("stratum", "global_accept_per_sec") => self.stratum_global_accept_per_sec.to_string(),
            ("stratum", "bans_enabled") => self.stratum_bans.enabled.to_string(),
            ("stratum", "ban_threshold") => self.stratum_bans.threshold.to_string(),
            ("stratum", "ban_soft_cap") => self.stratum_bans.soft_cap.to_string(),
            ("stratum", "ban_decay_secs") => self.stratum_bans.decay_secs.to_string(),
            ("stratum", "ban_base_secs") => self.stratum_bans.base_secs.to_string(),
            ("stratum", "ban_ladder_factor") => self.stratum_bans.ladder_factor.to_string(),
            ("stratum", "ban_max_secs") => self.stratum_bans.max_secs.to_string(),
            ("stratum", "ban_table_entries") => self.stratum_bans.table_entries.to_string(),
            ("stratum", "garbage_throttle_secs") => (self.stratum_bans.throttle_ms / 1000).to_string(),
            ("stratum", "submit_rate_per_sec") => (self.stratum_rates.submit_per_sec as u64).to_string(),
            ("stratum", "submit_burst") => (self.stratum_rates.submit_burst as u64).to_string(),
            ("stratum", "line_rate_per_sec") => (self.stratum_rates.line_per_sec as u64).to_string(),
            ("stratum", "line_burst") => (self.stratum_rates.line_burst as u64).to_string(),
            ("stratum", "auth_deadline_secs") => (self.stratum_auth_deadline_ms / 1000).to_string(),
            ("stratum", "idle_evict_secs") => (self.stratum_idle_evict_ms / 1000).to_string(),
            ("stratum", "read_deadline_secs") => (self.stratum_read_deadline_ms / 1000).to_string(),
            ("stratum", "write_timeout_secs") => (self.stratum_write_timeout_ms / 1000).to_string(),
            ("stratum", "out_buf_cap_bytes") => self.stratum_out_buf_cap.to_string(),
            ("stratum", "vardiff_setpoint_secs") => self.stratum_setpoint_secs.to_string(),
            ("stratum", "vardiff_start_diff") => self.stratum_diff.start_diff.to_string(),
            ("stratum", "vardiff_min_diff") => self.stratum_diff.min_diff.to_string(),
            ("stratum", "vardiff_tick_secs") => (self.stratum_tick_ms / 1000).to_string(),
            ("stratum", "vardiff_retarget_gate_shares") => self.stratum_cadence.retarget_gate_shares.to_string(),
            ("stratum", "vardiff_retarget_gate_secs") => (self.stratum_cadence.retarget_gate_secs as u64).to_string(),
            ("stratum", "vardiff_warmup_shares") => self.stratum_cadence.warmup_shares.to_string(),
            ("stratum", "vardiff_warmup_gate_shares") => self.stratum_cadence.warmup_gate_shares.to_string(),
            ("stratum", "vardiff_mature_shares") => self.stratum_cadence.mature_shares.to_string(),
            ("stratum", "vardiff_max_step") => (self.stratum_cadence.max_step as u64).to_string(),
            ("stratum", "vardiff_enabled") => self.stratum_vardiff_enabled.to_string(),
            ("stratum", "vardiff_fixed_diff") => self.stratum_vardiff_fixed_diff.to_string(),
            ("stratum", "vardiff_max_diff") => self.stratum_vardiff_max_diff.to_string(),
            ("stratum", "vardiff_dead_zone_pct") => ((self.stratum_cadence.dead_zone * 100.0).round() as i64).to_string(),
            ("stratum", "vardiff_mature_zone_pct") => ((self.stratum_cadence.mature_zone * 100.0).round() as i64).to_string(),
            ("stratum", "vardiff_fast_escape_pct") => ((self.stratum_cadence.fast_escape * 100.0).round() as i64).to_string(),
            ("stratum", "vardiff_silence_slack") => (self.stratum_cadence.silence_slack as i64).to_string(),
            ("stratum", "vardiff_dsps_tau_fast_secs") => (self.stratum_cadence.tau[0] as i64).to_string(),
            ("stratum", "vardiff_dsps_tau_mid_secs") => (self.stratum_cadence.tau[1] as i64).to_string(),
            ("stratum", "vardiff_dsps_tau_slow_secs") => (self.stratum_cadence.tau[2] as i64).to_string(),
            ("stratum", "vardiff_ladder") => format!(
                "{:?}",
                (0..self.stratum_cadence.ladder_len)
                    .map(|i| (self.stratum_cadence.ladder[i] * 10.0).round() as i64)
                    .collect::<Vec<_>>()
            ),
            ("stratum", "diff_cache_entries") => self.stratum_diff.cache_entries.to_string(),
            ("stratum", "diff_cache_ttl_secs") => (self.stratum_diff.cache_ttl_ms / 1000).to_string(),
            ("stratum", "reconnect_storm_per_min") => self.stratum_diff.storm_per_min.to_string(),
            ("stratum", "reconnect_storm_window_secs") => (self.stratum_diff.storm_window_ms / 1000).to_string(),
            ("rpc", "listen") => format!("{:?}", self.rpc_listen.to_string()),

            ("rpc", "token") => {
                if self.rpc_token.is_some() { "\"<set, not shown>\"".into() } else { "\"\"".into() }
            }
            ("mempool", "relay_fee_mile") => self.relay_fee_mile.to_string(),
            ("mempool", "max_txs") => self.mempool_max_txs.to_string(),
            ("mining", "author_note") => {
                format!("{:?}", plaine_rpc::notes::for_log(&self.author_note, 64))
            }
            ("checkpoints", "enabled") => self.checkpoints.is_enabled().to_string(),
            ("checkpoints", "threshold") => self.checkpoint_threshold.to_string(),
            ("checkpoints", "keys") => format!("{:?}", self.checkpoints.fingerprints()),
            ("author", "pubkey") => {
                format!("\"{}...\"", embedded::fingerprint(&self.author_pubkey))
            }
            ("author", "show_in_log") => self.author_show_in_log.to_string(),
            ("log", "level") => format!("{:?}", self.log_level.as_str()),
            _ => "?".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Config {
        load("noded.toml", src, &Overrides::default())
            .unwrap_or_else(|e| panic!("should load:\n{e}"))
    }

    fn err(src: &str) -> Diagnostic {
        load("noded.toml", src, &Overrides::default()).expect_err("should not load")
    }

    #[test]
    fn absent_file_is_valid() {
        let c = ok("");
        assert_eq!(c.network, Network::Main);
        assert_eq!(c.rpc_listen.port(), k::PORT_RPC);
        assert_eq!(c.p2p_listen.port(), k::PORT_P2P);
        assert_eq!(c.stratum_listen.port(), k::PORT_STRATUM);
        assert!(c.rpc_token.is_none());
        assert!(c.prune);
        assert!(!c.txindex);
        assert_eq!(c.log_level, LogLevel::Info);

        assert!(matches!(c.checkpoints, CheckpointPolicy::Embedded { .. }));
        assert_eq!(c.checkpoints.keys(), vec![embedded::CHECKPOINT_AUTHORITY_KEY.bytes]);
        assert_eq!(c.author_pubkey, embedded::AUTHOR_KEY.bytes);
        assert_eq!(c.author_key_source, plaine_rpc::views::KeySource::Embedded);
    }

    #[test]
    fn network_conversions_not_reversed() {
        use plaine_consensus::constants::Network as Consensus;

        assert_eq!(Network::Main.to_consensus(), Consensus::Main);
        assert_eq!(&Network::Main.to_consensus().chain_id(), b"PLNE");
        assert_eq!(Network::Main.to_rpc(), plaine_rpc::Network::Main);

        for n in [Network::Main] {
            assert_eq!(n.to_consensus().as_str(), n.as_str());
            assert_eq!(n.to_rpc().as_str(), n.as_str());
            assert_eq!(n.to_consensus().magic(), k::MAGIC_MAIN);
        }
    }

    #[test]
    fn configured_network_reaches_params() {
        let m = ok("[node]
network = \"main\"
");
        assert_eq!(m.network, Network::Main);
        assert_eq!(
            crate::node::chain_params(&m, 2).network,
            plaine_consensus::constants::Network::Main
        );
    }

    #[test]
    fn absent_seeds_uses_embedded_table() {
        let main = ok("");
        assert_eq!(main.seeds, embedded::SEEDS_MAIN.hosts.to_vec());
        assert!(!main.seeds.is_empty(), "a stock node must have somewhere to call");
    }

    #[test]
    fn empty_seeds_uses_embedded_table() {
        let c = ok("[p2p]\nseeds = []\n");
        assert_eq!(c.seeds, embedded::SEEDS_MAIN.hosts.to_vec());
    }

    #[test]
    fn explicit_seeds_win() {
        let c = ok("[p2p]\nseeds = [\"10.44.0.11:9256\", \"seed.example.net\"]\n");
        assert_eq!(c.seeds, vec!["10.44.0.11:9256".to_string(), "seed.example.net".to_string()]);
    }

    #[test]
    fn no_seeds_is_expressible() {
        let c = ok("[p2p]\nuse_embedded_seeds = false\n");
        assert!(c.seeds.is_empty());
        assert!(
            c.warnings.iter().any(|w| w.contains("will not dial anybody")),
            "silently dialling nobody is the defect: {:?}",
            c.warnings
        );

        let both = ok("[p2p]\nuse_embedded_seeds = false\nseeds = [\"10.44.0.11:9256\"]\n");
        assert_eq!(both.seeds, vec!["10.44.0.11:9256".to_string()]);
        assert!(!both.warnings.iter().any(|w| w.contains("will not dial anybody")));
    }

    #[test]
    fn placeholder_seeds_warn() {
        let c = ok("");
        let warned = c.warnings.iter().any(|w| w.contains("BUILD PLACEHOLDER"));
        assert_eq!(
            warned,
            embedded::SEEDS_MAIN.placeholder,
            "the placeholder warning and the placeholder table must agree"
        );
    }

    #[test]
    fn pasted_placeholder_key_warns() {
        let cp = plaine_consensus::hex::encode(&embedded::RETIRED_CHECKPOINT_PLACEHOLDER);
        let au = plaine_consensus::hex::encode(&embedded::RETIRED_AUTHOR_PLACEHOLDER);

        let c = ok(&format!("[checkpoints]\nkeys = [\"{cp}\"]\n[author]\npubkey = \"{au}\"\n"));

        assert!(matches!(c.checkpoints, CheckpointPolicy::Configured { .. }));
        assert_eq!(c.author_key_source, plaine_rpc::views::KeySource::Config);

        assert!(
            c.warnings.iter().any(|w| w.contains("BUILD PLACEHOLDER") && w.contains("checkpoint")),
            "a pasted placeholder checkpoint key bought silence: {:?}",
            c.warnings
        );
        assert!(
            c.warnings.iter().any(|w| w.contains("BUILD PLACEHOLDER") && w.contains("author")),
            "a pasted placeholder author key bought silence: {:?}",
            c.warnings
        );

        let out = c.print_effective(&Paths::resolve(&c.data_dir, None));
        assert!(out.contains("BUILD PLACEHOLDER"), "{out}");
        assert!(out.contains("PLACEHOLDER)"), "author line lost its marker:\n{out}");

        let mut other = embedded::RETIRED_CHECKPOINT_PLACEHOLDER;
        other[31] = b'4';
        let c = ok(&format!(
            "[checkpoints]\nkeys = [\"{}\"]\n",
            plaine_consensus::hex::encode(&other)
        ));
        assert!(
            !c.warnings.iter().any(|w| w.contains("checkpoint key in force")),
            "{:?}",
            c.warnings
        );
    }

    #[test]
    fn default_file_roundtrips() {
        for net in [Network::Main] {
            let text = default_config_text(net);
            let from_file = ok(&text);
            let from_nothing = ok("");
            assert_eq!(from_file.network, from_nothing.network, "{}", net.as_str());
            assert_eq!(from_file.rpc_listen, from_nothing.rpc_listen);
            assert_eq!(from_file.checkpoints, from_nothing.checkpoints);
            assert_eq!(from_file.author_pubkey, from_nothing.author_pubkey);
            assert!(from_file.provenance.iter().all(|(_, _, s)| *s == Source::Default));
        }
    }

    #[test]
    fn private_peers_allowed() {
        assert!(
            !ok("[node]
network = \"main\"
").accept_local_addrs,
            "the default must stay: a gossiped private address is a claim"
        );
        assert!(ok("[p2p]
accept_local_addrs = true
").accept_local_addrs);
        assert!(!ok("[p2p]
accept_local_addrs = false
").accept_local_addrs);
    }

    #[test]
    fn empty_key_list_uses_embedded() {
        assert!(matches!(ok("").checkpoints, CheckpointPolicy::Embedded { .. }));

        let c = ok("[checkpoints]\nkeys = []\n");
        assert!(matches!(c.checkpoints, CheckpointPolicy::Embedded { .. }));
        assert!(c.checkpoints.is_enabled());
        assert_eq!(c.checkpoints.keys().len(), 1);
    }

    #[test]
    fn only_enabled_flag_disables() {
        let c = ok("[checkpoints]\nenabled = false\n");
        assert_eq!(c.checkpoints, CheckpointPolicy::Disabled);
        assert!(c.checkpoints.keys().is_empty());

        assert!(c.checkpoints.describe().contains("OFF"));
        assert!(c.warnings.iter().any(|w| w.contains("Checkpoints are off")));
    }

    #[test]
    fn key_states_are_distinct() {
        for attempt in ["[checkpoints]\nkeys = []\n", "[checkpoints]\nkeys = [ ]\n"] {
            assert!(
                ok(attempt).checkpoints.is_enabled(),
                "emptying `keys` must never disable the layer: {attempt}"
            );
        }

        let e = err("[checkpoints]\nkeys = [\"\"]\n");
        assert!(e.message.contains("empty string"), "{e}");
        let help = e.help.unwrap();
        assert!(help.contains("enabled = false"));
        assert!(help.contains("keys = []"));
    }

    #[test]
    fn contradictory_file_refused() {
        let key = plaine_consensus::hex::encode(&embedded::CHECKPOINT_AUTHORITY_KEY.bytes);
        let e = err(&format!("[checkpoints]\nenabled = false\nkeys = [\"{key}\"]\n"));
        assert!(e.message.contains("two opposite things"), "{e}");
    }

    #[test]
    fn configured_key_replaces_embedded() {
        let key = plaine_consensus::hex::encode(&embedded::AUTHOR_KEY.bytes);
        let c = ok(&format!("[checkpoints]\nkeys = [\"{key}\"]\n"));
        assert!(matches!(c.checkpoints, CheckpointPolicy::Configured { .. }));
        assert_eq!(c.checkpoints.source(), plaine_rpc::views::KeySource::Config);
        assert_eq!(c.checkpoints.keys(), vec![embedded::AUTHOR_KEY.bytes]);
    }

    #[test]
    fn stratum_limits_default_to_todays_values() {
        let c = ok("");
        assert!(c.stratum_enforcement);
        assert!(c.stratum_vardiff_enabled);
        assert_eq!(c.stratum_bans.threshold, 100);
        assert_eq!(c.stratum_rates.submit_per_sec as u64, 3);
        assert_eq!(c.stratum_diff.min_diff, 8192);
        assert_eq!(c.stratum_diff.start_diff, 60000);
        assert_eq!(c.stratum_cadence.max_step as u64, 4);
        assert_eq!(c.stratum_cadence.dead_zone, 1.5);
        assert_eq!(c.stratum_cadence.ladder_len, 10);
        assert_eq!(c.stratum_auth_deadline_ms, 10_000);
    }

    #[test]
    fn stratum_enforcement_off_disables_policy_but_not_vardiff() {
        let c = ok("[stratum]\nenforcement = false\n");
        assert!(!c.stratum_bans.enabled);
        assert!(!c.stratum_rates.submit_enabled);
        assert!(!c.stratum_rates.line_enabled);
        assert!(!c.stratum_diff.storm_enabled);
        assert_eq!(c.stratum_max_per_ip, 0);
        assert_eq!(c.stratum_global_accept_per_sec, 0);
        assert_eq!(c.stratum_auth_deadline_ms, 0);
        assert_eq!(c.stratum_idle_evict_ms, 0);
        assert_eq!(c.stratum_out_buf_cap, 0);
        assert!(c.stratum_vardiff_enabled, "difficulty adjustment is not a policing knob");
    }

    #[test]
    fn vardiff_off_switch_and_shape_knobs_parse() {
        let c = ok("[stratum]\nvardiff_enabled = false\nvardiff_fixed_diff = 40000\nvardiff_dead_zone_pct = 200\nvardiff_max_step = 8\nvardiff_ladder = [10, 20, 50]\n");
        assert!(!c.stratum_vardiff_enabled);
        assert_eq!(c.stratum_vardiff_fixed_diff, 40000);
        assert_eq!(c.stratum_cadence.dead_zone, 2.0);
        assert_eq!(c.stratum_cadence.max_step as u64, 8);
        assert_eq!(c.stratum_cadence.ladder_len, 3);
        assert_eq!(c.stratum_cadence.ladder[1], 2.0);
    }

    #[test]
    fn bad_vardiff_ladder_refused() {
        assert!(err("[stratum]\nvardiff_ladder = [10, 10]\n").message.contains("increase"));
        assert!(err("[stratum]\nvardiff_ladder = [5]\n").message.contains("10..=999"));
        assert!(err("[stratum]\nvardiff_ladder = []\n").message.contains("entries"));
        assert!(err("[stratum]\nvardiff_ladder = [\"a\"]\n").message.contains("integer"));
    }

    #[test]
    fn zero_sentinel_disables_limit() {
        let c = ok("[stratum]\nsubmit_rate_per_sec = 0\nmax_per_ip = 0\nidle_evict_secs = 0\n");
        assert!(!c.stratum_rates.submit_enabled);
        assert_eq!(c.stratum_max_per_ip, 0);
        assert_eq!(c.stratum_idle_evict_ms, 0);
    }

    #[test]
    fn mistyped_key_caught_at_startup() {
        let e = err(&format!("[checkpoints]\nkeys = [\"{}\"]\n", "00".repeat(32)));
        assert!(e.message.contains("not a valid ed25519 public key"), "{e}");
        assert!(e.note.unwrap().contains("you would believe you were protected"));

        let e2 = err("[checkpoints]\nkeys = [\"deadbeef\"]\n");
        assert!(e2.message.contains("64 hex characters"));

        let e3 = err(&format!("[checkpoints]\nkeys = [\"{}\"]\n", "z".repeat(64)));
        assert!(e3.message.contains("hexadecimal"));
    }

    #[test]
    fn unmeetable_threshold_refused() {
        let e = err("[checkpoints]\nthreshold = 2\n");
        assert!(e.message.contains("only 1 key"), "{e}");
        assert!(e.help.unwrap().contains("would be off without saying so"));
    }

    #[test]
    fn author_key_two_states() {
        assert_eq!(ok("").author_key_source, plaine_rpc::views::KeySource::Embedded);
        assert_eq!(
            ok("[author]\npubkey = \"\"\n").author_pubkey,
            embedded::AUTHOR_KEY.bytes
        );

        let key = plaine_consensus::hex::encode(&embedded::CHECKPOINT_AUTHORITY_KEY.bytes);
        let c = ok(&format!("[author]\npubkey = \"{key}\"\n"));
        assert_eq!(c.author_key_source, plaine_rpc::views::KeySource::Config);
        assert!(!c.author_key_placeholder);

        let e = err("[author]\nenabled = false\n");
        assert!(e.message.contains("unknown key `enabled`"), "{e}");
    }

    #[test]
    fn announce_log_off_warns() {
        let c = ok("[author]\nshow_in_log = false\n");
        assert!(c.warnings.iter().any(|w| w.contains("emergency announcements")));
    }

    #[test]
    fn key_typo_names_intended_key() {
        let e = err("[p2p]\nmax_peer = 64\n");
        assert!(e.message.contains("unknown key `max_peer`"), "{e}");
        assert!(e.help.as_deref().unwrap().contains("max_peers"));
        let rendered = e.to_string();
        assert!(rendered.contains("  --> noded.toml:2:1"));
        assert!(rendered.contains("help: did you mean `max_peers`?"));
    }

    #[test]
    fn section_typo_names_intended_section() {
        let e = err("[rcp]\nlisten = \"127.0.0.1:9257\"\n");
        assert!(e.message.contains("unknown section [rcp]"), "{e}");
        assert!(e.help.unwrap().contains("[rpc]"));
    }

    #[test]
    fn wrong_type_names_expected() {
        let e = err("[p2p]\nmax_peers = \"many\"\n");
        assert!(e.message.contains("takes an integer"), "{e}");
        assert!(e.message.contains("is a string"));
    }

    #[test]
    fn address_without_port_refused() {
        let e = err("[rpc]\nlisten = \"127.0.0.1\"\n");
        assert!(e.help.as_deref().unwrap().contains("port is missing"), "{e}");
    }

    #[test]
    fn out_of_range_carries_reason() {
        let e = err("[stratum]\nmax_connections = 20000\n");
        assert!(e.message.contains("outside 1..=8192"), "{e}");
        assert!(e.note.unwrap().contains("LimitNOFILE"));
        let e2 = err("[mempool]\nrelay_fee_mile = 0\n");
        assert!(e2.note.unwrap().contains("consensus floor is 1 mile"));
    }

    #[test]
    fn public_rpc_without_token_refused() {
        let e = err("[rpc]\nlisten = \"0.0.0.0:9257\"\n");
        assert!(e.message.contains("not safe as configured"), "{e}");
        let help = e.help.unwrap();
        assert!(help.contains("ssh -L"), "the fix must be in the message");
        assert!(help.contains("token"));
    }

    #[test]
    fn public_rpc_with_token_warns_tls() {
        let c = ok(&format!(
            "[rpc]\nlisten = \"0.0.0.0:9257\"\ntoken = \"{}\"\n",
            "k".repeat(40)
        ));
        assert!(c.rpc_token.is_some());
        assert!(c.warnings.iter().any(|w| w.contains("clear text")));
    }

    #[test]
    fn token_never_printed() {
        let c = ok(&format!(
            "[rpc]\nlisten = \"0.0.0.0:9257\"\ntoken = \"{}\"\n",
            "secret-token-secret-token-secret-token"
        ));
        let paths = Paths::resolve(&c.data_dir, None);
        let printed = c.print_effective(&paths);
        assert!(!printed.contains("secret-token"), "the token must not be echoed:\n{printed}");
        assert!(printed.contains("<set, not shown>"));
    }

    #[test]
    fn overlong_author_note_refused() {
        let e = err(&format!("[mining]\nauthor_note = \"{}\"\n", "x".repeat(300)));
        assert!(e.message.contains("300 bytes"), "{e}");
        assert!(e.note.unwrap().contains("bytes, not characters"));
    }

    #[test]
    fn author_note_warns_linkage() {
        let c = ok("[mining]\nauthor_note = \"mined by me\"\n");
        assert!(c.warnings.iter().any(|w| w.contains("links every block")));
    }

    #[test]
    fn cli_overrides_beat_file() {
        let ov = Overrides {
            data_dir: Some(PathBuf::from("/elsewhere")),
            ..Default::default()
        };
        let c = load("noded.toml", "[node]\ndata_dir = \"/in-file\"\n", &ov).expect("load");
        assert_eq!(c.data_dir, PathBuf::from("/elsewhere"));
        assert!(c
            .provenance
            .iter()
            .any(|(s, k, src)| s == "node" && k == "data_dir" && *src == Source::Cli));
    }

    #[test]
    fn print_effective_shows_provenance() {
        let c = ok("[node]\nprune = false\n");
        let paths = Paths::resolve(&c.data_dir, None);
        let out = c.print_effective(&paths);
        assert!(out.contains("prune = false  # file"));
        assert!(out.contains("txindex = false  # default"));
        assert!(out.contains(&format!("height {}", k::CHECKPOINT_SUNSET_HEIGHT)));
        assert!(out.contains("config cannot change it"));
    }

    #[test]
    fn resolved_author_key_reaches_consensus() {

        use plaine_consensus::ed25519_dalek::{Signer, SigningKey};

        let sk = SigningKey::from_bytes(&[0x11u8; 32]);
        let pk: [u8; 32] = sk.verifying_key().to_bytes();

        let cfg = ok(&format!("[author]\npubkey = \"{}\"\n", plaine_consensus::hex::encode(&pk)));
        assert_eq!(cfg.author_pubkey, pk);
        assert_eq!(cfg.author_key_source, plaine_rpc::views::KeySource::Config);

        let (fee, nonce, encoding) = (k::FEE_FLOOR_MILE, 0u64, 0x01u8);
        let payload = b"emergency: Isochron v2 activates at height 600000".to_vec();

        let net = cfg.network.to_consensus();
        let msg = plaine_consensus::crypto::announcement_signing_message(
            net, &pk, fee, nonce, encoding, &payload,
        )
        .expect("payload length is in 1..=1024");
        let sig = sk.sign(&msg).to_bytes();
        let tx = plaine_consensus::codec::AnnouncementTx {
            from_pub: pk,
            fee,
            nonce,
            encoding,
            payload,
            sig,
        };

        assert_eq!(
            plaine_consensus::tx::check_announcement_stateless(net, &tx, &cfg.author_pubkey),
            Ok(())
        );

        let defaults = ok("");
        assert_eq!(defaults.author_pubkey, embedded::AUTHOR_KEY.bytes);
        assert_eq!(
            plaine_consensus::tx::check_announcement_stateless(net, &tx, &defaults.author_pubkey),
            Err(plaine_consensus::tx::TxError::NotAuthorKey)
        );
    }
}
