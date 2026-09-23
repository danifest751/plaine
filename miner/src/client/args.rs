pub mod batch;
pub mod bench;
pub mod clock;
pub mod conf;
pub mod cpu;
pub mod pads;
pub mod topo;

use crate::client::work;
use crate::client::Args;
use pads::Ask;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolUrl {
    pub stratum: String,
    pub login: String,
}

pub const DEFAULT_PORT: u16 = 9258;

pub const DEFAULT_HOST: &str = "127.0.0.1";

pub const DEFAULT_BENCH_SECS: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Help,
    Version,
    PrintTopology,
    Bench,
    Grind(String),
    Mine,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub action: Action,
    pub client: Args,
    pub cpus: Option<Vec<usize>>,
    pub priority: Option<u8>,
    pub pages: Ask,
    pub bench_secs: u64,
    pub config_path: Option<String>,
    /// An explicit --batch N. None means the machine decides; see `args::batch`.
    pub batch: Option<usize>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            action: Action::Mine,
            client: Args::default(),
            cpus: None,
            priority: None,
            pages: Ask::Auto,
            bench_secs: DEFAULT_BENCH_SECS,
            config_path: None,
            batch: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Partial {
    pub login: Option<String>,
    pub stratum: Option<String>,
    pub threads: Option<usize>,
    pub cpus: Option<Vec<usize>>,
    pub priority: Option<u8>,
    pub pages: Option<Ask>,
    pub status_secs: Option<u64>,
    pub max_shares: Option<u64>,
    pub max_blocks: Option<u64>,
    pub deadline_secs: Option<u64>,
    pub max_reconnects: Option<u32>,
    pub reconnect: Option<bool>,
    pub verbose: Option<bool>,
    pub bench: Option<bool>,
    pub bench_secs: Option<u64>,
    pub batch: Option<usize>,
    pub print_topology: Option<bool>,
    pub grind: Option<String>,
    pub config: Option<String>,
    pub help: bool,
    pub version: bool,
}

impl Partial {
    pub fn under(self, lower: Partial) -> Partial {
        Partial {
            login: self.login.or(lower.login),
            stratum: self.stratum.or(lower.stratum),
            threads: self.threads.or(lower.threads),
            cpus: self.cpus.or(lower.cpus),
            priority: self.priority.or(lower.priority),
            pages: self.pages.or(lower.pages),
            status_secs: self.status_secs.or(lower.status_secs),
            max_shares: self.max_shares.or(lower.max_shares),
            max_blocks: self.max_blocks.or(lower.max_blocks),
            deadline_secs: self.deadline_secs.or(lower.deadline_secs),
            max_reconnects: self.max_reconnects.or(lower.max_reconnects),
            reconnect: self.reconnect.or(lower.reconnect),
            verbose: self.verbose.or(lower.verbose),
            bench: self.bench.or(lower.bench),
            bench_secs: self.bench_secs.or(lower.bench_secs),
            batch: self.batch.or(lower.batch),
            print_topology: self.print_topology.or(lower.print_topology),
            grind: self.grind.or(lower.grind),
            config: self.config.or(lower.config),
            help: self.help || lower.help,
            version: self.version || lower.version,
        }
    }
}

pub fn parse(argv: &[String], notes: &mut Vec<String>) -> Result<Options, String> {
    let cli = parse_argv(argv)?;
    if cli.help {
        return Ok(Options { action: Action::Help, ..Options::default() });
    }
    if cli.version {
        return Ok(Options { action: Action::Version, ..Options::default() });
    }

    let file = match &cli.config {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("--config {path}: {e}"))?;
            let p = from_config(&conf::parse(&text).map_err(|e| format!("{path}: {e}"))?)
                .map_err(|e| format!("{path}: {e}"))?;

            if p.threads.is_some() && cli.cpus.is_some() {
                notes.push(format!(
                    "--cpu-affinity on the command line overrides \"threads\" from {path}"
                ));
            }
            if p.cpus.is_some() && cli.threads.is_some() {
                notes.push(format!(
                    "--threads on the command line overrides \"cpu-affinity\" from {path}"
                ));
            }
            p
        }
        None => Partial::default(),
    };
    let config_path = cli.config.clone();

    let mut merged = cli.clone().under(file);
    if cli.cpus.is_some() {
        merged.threads = cli.threads;
    }
    if cli.threads.is_some() {
        merged.cpus = cli.cpus.clone();
    }
    resolve(merged, config_path, notes)
}

fn resolve(
    p: Partial,
    config_path: Option<String>,
    notes: &mut Vec<String>,
) -> Result<Options, String> {
    if p.threads.is_some() && p.cpus.is_some() {
        return Err(
            "--threads and --cpu-affinity conflict: --threads N asks for N workers wherever \
             the OS puts them, --cpu-affinity LIST asks for one worker per named CPU and \
             already says how many. Use one."
                .into(),
        );
    }

    let mut client = Args::default();
    if let Some(v) = p.login {
        client.login = v;
    }
    if let Some(v) = p.stratum {
        client.stratum = v;
    }
    if let Some(v) = p.status_secs {
        client.status_secs = v;
    }
    if let Some(v) = p.max_shares {
        client.max_shares = v;
    }
    if let Some(v) = p.max_blocks {
        client.max_blocks = v;
    }
    if let Some(v) = p.deadline_secs {
        client.deadline_secs = v;
    }
    if let Some(v) = p.max_reconnects {
        client.max_reconnects = v;
    }
    if let Some(v) = p.reconnect {
        client.reconnect = v;
    }
    if let Some(v) = p.verbose {
        client.verbose = v;
    }

    client.huge_pages = p.pages.unwrap_or_default().wanted();
    client.threads = match (&p.cpus, p.threads) {
        (Some(list), _) => list.len(),
        (None, Some(n)) => n.max(1),
        (None, None) => client.threads,
    };
    if p.threads == Some(0) {
        notes.push("--threads 0 means one worker".into());
    }
    if client.threads > work::MAX_WORKERS {
        let asked = client.threads;
        client.threads = work::MAX_WORKERS;
        notes.push(format!(
            "{asked} workers asked for, {} used: a worker owns one of {} nonce lanes, and              workers past that repeat an earlier lane's nonces, which the server counts as              duplicate shares and bans for",
            work::MAX_WORKERS,
            work::MAX_WORKERS
        ));
    }

    let action = if p.print_topology == Some(true) {
        Action::PrintTopology
    } else if p.bench == Some(true) {
        Action::Bench
    } else if let Some(hex) = p.grind {
        Action::Grind(hex)
    } else {
        Action::Mine
    };

    if let Some(level) = p.priority {
        if level > cpu::PRIORITY_LEVELS {
            return Err(format!(
                "--cpu-priority takes 0..{}, not {level} (0 idle, 2 normal, {} highest)",
                cpu::PRIORITY_LEVELS,
                cpu::PRIORITY_LEVELS
            ));
        }
    }
    let bench_secs = p.bench_secs.unwrap_or(DEFAULT_BENCH_SECS);
    if bench_secs == 0 {
        return Err("--bench-seconds needs at least 1".into());
    }

    Ok(Options {
        action,
        client,
        cpus: p.cpus,
        priority: p.priority,
        pages: p.pages.unwrap_or_default(),
        bench_secs,
        config_path,
        batch: p.batch,
    })
}

fn parse_argv(argv: &[String]) -> Result<Partial, String> {
    let mut p = Partial::default();
    let mut i = 0usize;
    while i < argv.len() {
        let raw = argv[i].as_str();
        let (name, inline) = match raw.starts_with("--").then(|| raw.split_once('=')).flatten() {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (raw, None),
        };
        i += 1;

        let mut value = |what: &str| -> Result<String, String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => {
                    let v = argv.get(i).cloned().ok_or(format!("{name} needs {what}"))?;
                    i += 1;
                    Ok(v)
                }
            }
        };
        let int = |v: &str, msg: &str| -> Result<u64, String> {
            v.trim().parse::<u64>().map_err(|_| msg.to_string())
        };

        match name {
            "-h" | "--help" => p.help = true,
            "-V" | "--version" => p.version = true,
            "-v" | "--verbose" => p.verbose = Some(true),
            "--address" => p.login = Some(value("a value")?),
            "--stratum" => p.stratum = Some(value("a value")?),
            "--config" => p.config = Some(value("a path")?),
            "--grind" => p.grind = Some(value("a 264-character hex header")?),
            "--threads" => {
                let v = value("a positive integer")?;
                p.threads = Some(int(&v, "--threads needs a positive integer")? as usize);
            }
            "--cpu-affinity" => {
                let v = value("a CPU list like 0,2,4,6 or 0-3,8-11")?;
                p.cpus = Some(parse_cpu_list(&v)?);
            }
            "--cpu-priority" => {
                let v = value("a level from 0 to 5")?;
                let n = int(&v, "--cpu-priority takes a level from 0 to 5")?;
                if n > cpu::PRIORITY_LEVELS as u64 {
                    return Err(format!(
                        "--cpu-priority takes 0..{}, not {n} (0 idle, 2 normal, {} highest)",
                        cpu::PRIORITY_LEVELS,
                        cpu::PRIORITY_LEVELS
                    ));
                }
                p.priority = Some(n as u8);
            }
            "--batch" => {
                let v = value("a slot count")?;
                p.batch = Some(batch::check(
                    int(&v, "--batch needs a positive integer")? as usize,
                )?);
            }
            "--huge-pages" => p.pages = Some(Ask::Force),
            "--no-huge-pages" => p.pages = Some(Ask::Never),
            "--print-topology" => p.print_topology = Some(true),
            "--bench" => p.bench = Some(true),
            "--bench-seconds" => {
                let v = value("a number of seconds")?;
                p.bench_secs = Some(int(&v, "--bench-seconds needs an integer")?);
            }
            "--max-shares" => {
                let v = value("an integer")?;
                p.max_shares = Some(int(&v, "--max-shares needs an integer")?);
            }
            "--max-blocks" => {
                let v = value("an integer")?;
                p.max_blocks = Some(int(&v, "--max-blocks needs an integer")?);
            }
            "--deadline" => {
                let v = value("an integer")?;
                p.deadline_secs = Some(int(&v, "--deadline needs an integer")?);
            }
            "--max-reconnects" => {
                let v = value("an integer")?;
                let n = int(&v, "--max-reconnects needs an integer")?;

                if n > u32::MAX as u64 {
                    return Err(format!("--max-reconnects {n} is larger than {}", u32::MAX));
                }
                p.max_reconnects = Some(n as u32);
            }
            "--status" => {
                let v = value("an integer number of seconds")?;
                p.status_secs = Some(int(&v, "--status needs an integer number of seconds")?);
            }
            "--no-reconnect" => p.reconnect = Some(false),

            other if !other.starts_with('-') => {
                let u = parse_pool_url(other)?;
                p.stratum = Some(u.stratum);
                p.login = Some(u.login);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(p)
}

fn from_config(pairs: &[(String, conf::Value)]) -> Result<Partial, String> {
    use conf::Value;
    let mut p = Partial::default();
    let mut seen: Vec<String> = Vec::new();

    for (raw_key, value) in pairs {
        let key = raw_key.replace('_', "-").to_ascii_lowercase();
        if seen.contains(&key) {
            return Err(format!("{raw_key:?} is set twice"));
        }
        seen.push(key.clone());

        let want = |k: &str| format!("{raw_key:?} takes {k}, not {}", value.kind());
        let num = |max: u64| -> Result<u64, String> {
            match value {
                Value::Num(n) if *n <= max => Ok(*n),
                Value::Num(n) => Err(format!("{raw_key:?} is {n}, which is out of range")),
                _ => Err(want("a number")),
            }
        };
        let boolean = || -> Result<bool, String> {
            match value {
                Value::Bool(b) => Ok(*b),
                _ => Err(want("true or false")),
            }
        };
        let text = || -> Result<String, String> {
            match value {
                Value::Str(s) => Ok(s.clone()),
                _ => Err(want("a string")),
            }
        };

        match key.as_str() {
            "url" => {
                let u = parse_pool_url(&text()?)?;
                p.stratum = Some(u.stratum);
                p.login = Some(u.login);
            }
            "address" => p.login = Some(text()?),
            "stratum" => p.stratum = Some(text()?),
            "threads" => p.threads = Some(num(u32::MAX as u64)? as usize),
            "batch" => p.batch = Some(batch::check(num(u32::MAX as u64)? as usize)?),
            "cpu-affinity" => {
                p.cpus = Some(match value {
                    Value::Str(s) => parse_cpu_list(s)?,
                    Value::List(v) => {
                        let joined =
                            v.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
                        parse_cpu_list(&joined)?
                    }
                    _ => return Err(want("a string like \"0-3,8-11\" or an array like [0,2,4]")),
                })
            }
            "cpu-priority" => p.priority = Some(num(cpu::PRIORITY_LEVELS as u64)? as u8),
            "huge-pages" => {
                p.pages = Some(if boolean()? { Ask::Force } else { Ask::Never })
            }
            "no-huge-pages" => p.pages = Some(if boolean()? { Ask::Never } else { Ask::Auto }),
            "status" => p.status_secs = Some(num(u64::MAX)?),
            "max-shares" => p.max_shares = Some(num(u64::MAX)?),
            "max-blocks" => p.max_blocks = Some(num(u64::MAX)?),
            "deadline" => p.deadline_secs = Some(num(u64::MAX)?),
            "max-reconnects" => p.max_reconnects = Some(num(u32::MAX as u64)? as u32),
            "no-reconnect" => p.reconnect = Some(!boolean()?),
            "verbose" => p.verbose = Some(boolean()?),
            "bench" => p.bench = Some(boolean()?),
            "bench-seconds" => p.bench_secs = Some(num(u64::MAX)?),
            "print-topology" => p.print_topology = Some(boolean()?),
            "grind" => p.grind = Some(text()?),
            "config" => {
                return Err(
                    "a config file cannot reference another config file"
                        .into(),
                )
            }
            _ => {
                return Err(format!(
                    "unknown key {raw_key:?}. The keys are the long flags without their `--`; \
                     `plaine-miner --help` lists them"
                ))
            }
        }
    }
    Ok(p)
}

pub fn parse_cpu_list(s: &str) -> Result<Vec<usize>, String> {
    const MAX_CPUS: usize = 4096;

    let mut out: Vec<usize> = Vec::new();
    let s = s.trim();
    if s.is_empty() {
        return Err("--cpu-affinity needs at least one CPU, like 0,2,4,6".into());
    }
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("{s:?} has an empty entry: two commas, or a trailing one"));
        }
        let one = |t: &str| -> Result<usize, String> {
            t.parse::<usize>().map_err(|_| format!("{t:?} is not a CPU number"))
        };
        let range: Vec<usize> = match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (one(a.trim())?, one(b.trim())?);
                if a > b {
                    return Err(format!("{part:?} counts down; write it as {b}-{a}"));
                }
                if b - a + 1 > MAX_CPUS {
                    return Err(format!("{part:?} names more than {MAX_CPUS} CPUs"));
                }
                (a..=b).collect()
            }
            None => vec![one(part)?],
        };
        for c in range {
            if out.contains(&c) {
                return Err(format!(
                    "CPU {c} appears twice in {s:?}; one worker is pinned per entry, so a \
                     repeat would double-book it"
                ));
            }
            if out.len() >= MAX_CPUS {
                return Err(format!("--cpu-affinity names more than {MAX_CPUS} CPUs"));
            }
            out.push(c);
        }
    }
    Ok(out)
}

pub fn check_cpu_list(list: &[usize], t: &topo::Topology) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    if t.source == "none" || t.source == "available_parallelism" {
        notes.push(format!(
            "the pin list was not checked against this machine: {}",
            t.notes.first().map(String::as_str).unwrap_or("no topology source")
        ));
        return Ok(notes);
    }
    for c in list {
        if t.cpu(*c).is_none() {
            return Err(format!(
                "CPU {c} does not exist on this machine; it has {}. \
                 `plaine-miner --print-topology` prints the map",
                topo::fmt_list(&t.cpus.iter().map(|x| x.id).collect::<Vec<_>>())
            ));
        }
    }
    if let Some(cores) = t.cores_covered(list) {
        if cores < list.len() {
            notes.push(format!(
                "{} of these {} CPUs are SMT siblings, so they share {cores} physical cores \
 - and on this machine siblings are numbered {}",
                list.len() - cores,
                list.len(),
                t.sibling_strides()
                    .iter()
                    .map(|k| format!("n/n+{k}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if t.hybrid() {
        let classes = t.classes();
        let used: Vec<u8> =
            classes.iter().copied().filter(|c| {
                list.iter().any(|id| t.cpu(*id).and_then(|x| x.class) == Some(*c))
            }).collect();
        if used.len() > 1 {
            notes.push(
                "this list mixes performance and efficiency cores, which differ by about 28% \
                 per clock at this pad size"
                    .into(),
            );
        }
    }
    Ok(notes)
}

pub fn parse_pool_url(arg: &str) -> Result<PoolUrl, String> {
    let s = arg.trim();
    if s.is_empty() {
        return Err("empty pool URL".into());
    }

    let rest = match s.split_once("://") {
        Some((scheme, rest)) => {
            let sc = scheme.to_ascii_lowercase();
            match sc.as_str() {
                "stratum+tcp" | "stratum" | "tcp" => {}
                "stratum+ssl" | "stratum+tls" | "stratums" | "ssl" | "tls" => {
                    return Err(format!(
                        "{sc}:// asks for an encrypted connection; this client speaks \
                         plaintext TCP only"
                    ))
                }
                other => return Err(format!("unknown URL scheme {other:?}")),
            }
            rest
        }
        None => s,
    };

    let rest = match rest.split_once('/') {
        Some((head, "")) => head,
        Some((_, tail)) => return Err(format!("a stratum URL has no path, found {tail:?}")),
        None => rest,
    };

    let (login, hostpart) = match rest.rsplit_once('@') {
        Some((l, h)) => (l, h),
        None => (rest, ""),
    };
    if login.is_empty() {
        return Err("no address in the pool URL: solo mining pays the coinbase to it".into());
    }

    let stratum = parse_host(hostpart)?;
    Ok(PoolUrl { stratum, login: login.to_string() })
}

fn parse_host(h: &str) -> Result<String, String> {
    if h.is_empty() {
        return Ok(format!("{DEFAULT_HOST}:{DEFAULT_PORT}"));
    }
    if let Some(close) = h.strip_prefix('[').and_then(|_| h.find(']')) {
        let after = &h[close + 1..];
        return match after {
            "" => Ok(format!("{h}:{DEFAULT_PORT}")),
            _ => match after.strip_prefix(':') {
                Some(p) => {
                    check_port(p)?;
                    Ok(h.to_string())
                }
                None => Err(format!("expected ':port' after ']', found {after:?}")),
            },
        };
    }

    if h.matches(':').count() > 1 {
        return Err(format!(
            "{h:?} looks like a bare IPv6 address; write it as [{h}]:{DEFAULT_PORT}"
        ));
    }
    match h.split_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err("no host before ':'".into());
            }
            check_port(port)?;
            Ok(h.to_string())
        }
        None => Ok(format!("{h}:{DEFAULT_PORT}")),
    }
}

fn check_port(p: &str) -> Result<(), String> {
    match p.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("{p:?} is not a port number")),
        Ok(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> PoolUrl {
        parse_pool_url(s).unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
    }

    fn args(v: &[&str]) -> Result<Options, String> {
        let argv: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        parse(&argv, &mut Vec::new())
    }

    // Same, but keeps the notes: some settings are adjusted rather than refused, and
    // then the note is the only thing that tells the operator what happened.
    fn args_with(v: &[&str], notes: &mut Vec<String>) -> Result<Options, String> {
        let argv: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        parse(&argv, notes)
    }

    #[test]
    fn zero_config_is_one_token() {
        let u = ok("plne1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq");
        assert_eq!(u.stratum, "127.0.0.1:9258");
        assert_eq!(u.login, "plne1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq");
    }

    #[test]
    fn rig_name_and_suffix_untouched() {
        assert_eq!(ok("plne1abc.rig1@h:1").login, "plne1abc.rig1");
        assert_eq!(ok("plne1abc.rig1+120000@h:1").login, "plne1abc.rig1+120000");
        assert_eq!(ok("plne1abc+8192").login, "plne1abc+8192");
    }

    #[test]
    fn host_and_port_split_on_last_at() {
        assert_eq!(ok("plne1abc.rig@pool.example").stratum, "pool.example:9258");
        assert_eq!(ok("plne1abc.rig@pool.example:3333").stratum, "pool.example:3333");
        assert_eq!(ok("stratum+tcp://plne1abc.rig@10.0.0.2:19258").stratum, "10.0.0.2:19258");
        assert_eq!(ok("stratum://plne1abc@h").stratum, "h:9258");
        assert_eq!(ok("tcp://plne1abc@h").stratum, "h:9258");

        assert_eq!(ok("stratum+tcp://plne1abc@h:1/").stratum, "h:1");
    }

    #[test]
    fn ipv6_needs_brackets() {
        assert_eq!(ok("plne1abc@[::1]").stratum, "[::1]:9258");
        assert_eq!(ok("plne1abc@[::1]:19258").stratum, "[::1]:19258");
        let e = parse_pool_url("plne1abc@::1").unwrap_err();
        assert!(e.contains("[::1]:9258"), "the error must show the fix: {e}");
    }

    #[test]
    fn encrypted_scheme_is_refused() {
        for s in ["stratum+ssl://plne1abc@h:1", "stratums://plne1abc@h:1", "tls://plne1abc@h:1"] {
            let e = parse_pool_url(s).unwrap_err();
            assert!(e.contains("plaintext"), "{s}: {e}");
        }
    }

    #[test]
    fn typos_are_refused() {
        assert!(parse_pool_url("").is_err());
        assert!(parse_pool_url("@host:1").is_err(), "no address");
        assert!(parse_pool_url("plne1abc@:1").is_err(), "no host");
        assert!(parse_pool_url("plne1abc@h:0").is_err(), "port 0");
        assert!(parse_pool_url("plne1abc@h:99999").is_err(), "port out of range");
        assert!(parse_pool_url("plne1abc@h:http").is_err(), "port not a number");
        assert!(parse_pool_url("http://plne1abc@h:1").is_err(), "wrong scheme");
        assert!(parse_pool_url("stratum+tcp://plne1abc@h:1/path").is_err(), "a path hides a typo");
    }

    #[test]
    fn rig_script_flags_still_parse() {
        let o = args(&[
            "plne1abc.rig1@pool:19258",
            "--threads", "6",
            "--status", "30",
            "--max-shares", "7",
            "--max-blocks", "2",
            "--deadline", "600",
            "--max-reconnects", "5",
            "--no-reconnect",
            "--verbose",
        ])
        .expect("parses");
        assert_eq!(o.action, Action::Mine);
        assert_eq!(o.client.login, "plne1abc.rig1");
        assert_eq!(o.client.stratum, "pool:19258");
        assert_eq!(o.client.threads, 6);
        assert_eq!(o.client.status_secs, 30);
        assert_eq!(o.client.max_shares, 7);
        assert_eq!(o.client.max_blocks, 2);
        assert_eq!(o.client.deadline_secs, 600);
        assert_eq!(o.client.max_reconnects, 5);
        assert!(!o.client.reconnect);
        assert!(o.client.verbose);

        let o = args(&["--address", "plne1x.rig", "--stratum", "h:1"]).unwrap();
        assert_eq!(o.client.login, "plne1x.rig");
        assert_eq!(o.client.stratum, "h:1");

        let d = args(&["plne1abc"]).unwrap();
        assert_eq!(d.client.status_secs, 10);
        assert!(d.client.reconnect);
        assert_eq!(d.client.stratum, "127.0.0.1:9258");
    }

    #[test]
    fn more_workers_than_nonce_lanes_are_capped() {
        // A worker owns one of 2^THREAD_BITS lanes. Ask for more and worker
        // MAX_WORKERS + k walks lane k's nonces exactly, which a server scores as
        // duplicate shares: +25 banscore each, banned after four. Dual-socket parts
        // with more than 256 threads exist and available_parallelism is the default,
        // so the cap cannot just be documented.
        let mut notes = Vec::new();
        let o = args_with(
            &["plne1abc", "--threads", &(work::MAX_WORKERS + 48).to_string()],
            &mut notes,
        )
        .expect("an oversized thread count is capped, not refused");
        assert_eq!(o.client.threads, work::MAX_WORKERS);
        assert!(
            notes.iter().any(|n| n.contains("duplicate shares")),
            "the cap has to say why, not silently drop workers: {notes:?}"
        );
    }

    #[test]
    fn a_thread_count_inside_the_lane_space_is_left_alone() {
        for n in [1usize, 2, 16, work::MAX_WORKERS - 1, work::MAX_WORKERS] {
            let o = args(&["plne1abc", "--threads", &n.to_string()]).expect("legal");
            assert_eq!(o.client.threads, n, "{n} workers fit and must not be touched");
        }
    }

    #[test]
fn threads_and_affinity_conflict() {

        let e = args(&["plne1abc", "--threads", "4", "--cpu-affinity", "0-3"]).unwrap_err();
        assert!(e.contains("--threads"), "{e}");
        assert!(e.contains("--cpu-affinity"), "{e}");

        assert!(args(&["plne1abc", "--cpu-affinity", "0-3", "--threads", "4"]).is_err());
    }

    #[test]
    fn pin_list_sets_worker_count() {
        let o = args(&["plne1abc", "--cpu-affinity", "0,2,4,6"]).unwrap();
        assert_eq!(o.cpus.as_deref(), Some(&[0usize, 2, 4, 6][..]));
        assert_eq!(o.client.threads, 4, "one worker per entry, and nothing else to say");
    }

    #[test]
    fn cpu_list_keeps_order_refuses_dupes() {
        assert_eq!(parse_cpu_list("0-3,8-11").unwrap(), vec![0, 1, 2, 3, 8, 9, 10, 11]);
        assert_eq!(parse_cpu_list("6,4,2,0").unwrap(), vec![6, 4, 2, 0], "order is meaning");
        assert_eq!(parse_cpu_list(" 1 , 3 ").unwrap(), vec![1, 3]);
        for bad in ["", "0,,2", "0,", "3-1", "a", "0-x", "0,0", "0-3,2"] {
            assert!(parse_cpu_list(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(parse_cpu_list("0-999999").is_err(), "a typo, not a topology");
    }

    #[test]
    fn modes_are_exclusive() {
        assert_eq!(args(&["--bench"]).unwrap().action, Action::Bench);
        assert_eq!(args(&["--print-topology"]).unwrap().action, Action::PrintTopology);
        assert_eq!(args(&["--bench"]).unwrap().client.login, "", "no address is needed");
        assert_eq!(args(&["--bench", "--bench-seconds", "3"]).unwrap().bench_secs, 3);
        assert_eq!(args(&["--bench"]).unwrap().bench_secs, DEFAULT_BENCH_SECS);
        assert!(args(&["--bench", "--bench-seconds", "0"]).is_err());

        assert_eq!(
            args(&["--bench", "--print-topology"]).unwrap().action,
            Action::PrintTopology
        );
        match args(&["--grind", "00ff"]).unwrap().action {
            Action::Grind(h) => assert_eq!(h, "00ff"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn equals_form_parses() {
        let o = args(&["--address=plne1x", "--threads=3", "--status=1"]).unwrap();
        assert_eq!(o.client.login, "plne1x");
        assert_eq!(o.client.threads, 3);
        assert_eq!(o.client.status_secs, 1);
    }

    #[test]
    fn huge_pages_default_is_try() {
        assert_eq!(args(&["plne1abc"]).unwrap().pages, Ask::Auto);
        assert_eq!(args(&["plne1abc", "--huge-pages"]).unwrap().pages, Ask::Force);
        assert_eq!(args(&["plne1abc", "--no-huge-pages"]).unwrap().pages, Ask::Never);
    }

    #[test]
    fn cpu_priority_is_range_checked() {
        assert_eq!(args(&["plne1abc", "--cpu-priority", "0"]).unwrap().priority, Some(0));
        assert_eq!(args(&["plne1abc", "--cpu-priority", "5"]).unwrap().priority, Some(5));
        assert!(args(&["plne1abc", "--cpu-priority", "6"]).is_err());
        assert!(args(&["plne1abc", "--cpu-priority", "-1"]).is_err());
        assert_eq!(args(&["plne1abc"]).unwrap().priority, None, "no change by default");
    }

    #[test]
    fn missing_value_names_the_flag() {
        for f in ["--threads", "--address", "--cpu-affinity", "--config", "--bench-seconds"] {
            let e = args(&[f]).unwrap_err();
            assert!(e.contains(f), "{f}: {e}");
        }
        assert!(args(&["--nonsense"]).unwrap_err().contains("nonsense"));
    }

    fn with_config(body: &str, extra: &[&str]) -> Result<Options, String> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("plaine-miner-test-{}-{seq}.json", std::process::id()));
        std::fs::write(&path, body).expect("write the test config");
        let mut argv: Vec<String> = vec!["--config".into(), path.display().to_string()];
        argv.extend(extra.iter().map(|s| s.to_string()));
        let r = parse(&argv, &mut Vec::new());
        let _ = std::fs::remove_file(&path);
        r
    }

    #[test]
    fn config_file_expresses_command_line() {
        let o = with_config(
            r#"{
                 "url": "plne1abc.rig1@pool.example:9258",
                 "cpu-affinity": "0-3",
                 "cpu-priority": 1,
                 "huge-pages": true,
                 "status": 45,
                 "no-reconnect": true,
                 "verbose": true,
                 "bench-seconds": 20
               }"#,
            &[],
        )
        .expect("parses");
        assert_eq!(o.client.login, "plne1abc.rig1");
        assert_eq!(o.client.stratum, "pool.example:9258");
        assert_eq!(o.cpus.as_deref(), Some(&[0usize, 1, 2, 3][..]));
        assert_eq!(o.client.threads, 4);
        assert_eq!(o.priority, Some(1));
        assert_eq!(o.pages, Ask::Force);
        assert_eq!(o.client.status_secs, 45);
        assert!(!o.client.reconnect);
        assert!(o.client.verbose);
        assert_eq!(o.bench_secs, 20);
    }

    #[test]
    fn command_line_overrides_file() {
        let o = with_config(
            r#"{"address": "plne1file", "threads": 2, "status": 5}"#,
            &["--address", "plne1cli", "--status", "9"],
        )
        .expect("parses");
        assert_eq!(o.client.login, "plne1cli");
        assert_eq!(o.client.status_secs, 9);
        assert_eq!(o.client.threads, 2, "what the command line did not say, the file keeps");
    }

    #[test]
    fn cli_pin_list_overrides_file_threads() {
        let mut notes = Vec::new();
        let path = std::env::temp_dir().join("plaine-miner-test-displace.json");
        std::fs::write(&path, r#"{"threads": 8}"#).unwrap();
        let argv: Vec<String> = vec![
            "--config".into(),
            path.display().to_string(),
            "plne1abc".into(),
            "--cpu-affinity".into(),
            "0,1".into(),
        ];
        let o = parse(&argv, &mut notes).expect("parses");
        let _ = std::fs::remove_file(&path);
        assert_eq!(o.client.threads, 2);
        assert_eq!(o.cpus.as_deref(), Some(&[0usize, 1][..]));
        assert!(notes.iter().any(|n| n.contains("overrides")), "{notes:?}");
    }

    #[test]
    fn unknown_config_key_is_named() {
        let e = with_config(r#"{"cpu_prority": 2}"#, &[]).unwrap_err();
        assert!(e.contains("cpu_prority"), "{e}");
        assert!(e.contains("--help"), "{e}: the message must say where the list is");

        assert_eq!(with_config(r#"{"cpu_priority": 3}"#, &[]).unwrap().priority, Some(3));
    }

    #[test]
    fn wrong_type_says_what_it_wanted() {
        let e = with_config(r#"{"threads": "four"}"#, &[]).unwrap_err();
        assert!(e.contains("a number"), "{e}");
        let e = with_config(r#"{"verbose": 1}"#, &[]).unwrap_err();
        assert!(e.contains("true or false"), "{e}");
    }

    #[test]
    fn nested_config_include_is_refused() {
        let e = with_config(r#"{"config": "other.json"}"#, &[]).unwrap_err();
        assert!(e.contains("cannot reference another config file"), "{e}");
    }

    #[test]
    fn missing_config_file_names_path() {
        let e = parse(
            &["--config".to_string(), "no/such/file.json".to_string()],
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(e.contains("no/such/file.json"), "{e}");
    }

    #[test]
    fn file_affinity_takes_string_or_array() {
        assert_eq!(
            with_config(r#"{"cpu-affinity": [0, 2, 4, 6]}"#, &[]).unwrap().cpus.as_deref(),
            Some(&[0usize, 2, 4, 6][..])
        );
        assert_eq!(
            with_config(r#"{"cpu-affinity": "0-3"}"#, &[]).unwrap().cpus.as_deref(),
            Some(&[0usize, 1, 2, 3][..])
        );
    }

    #[test]
    fn file_both_thread_controls_conflict() {
        let e = with_config(r#"{"threads": 4, "cpu-affinity": "0-3"}"#, &[]).unwrap_err();
        assert!(e.contains("--cpu-affinity"), "{e}");
    }

    #[test]
    fn page_size_choice_reaches_workers() {
        assert!(args(&["plne1abc"]).unwrap().client.huge_pages, "the default is auto, i.e. try");
        assert!(args(&["plne1abc", "--huge-pages"]).unwrap().client.huge_pages);
        assert!(!args(&["plne1abc", "--no-huge-pages"]).unwrap().client.huge_pages);
        assert!(!with_config(r#"{"huge-pages": false}"#, &[]).unwrap().client.huge_pages);
    }

    #[test]
    fn pin_list_checked_against_machine() {
        let t = topo::Topology::detect();
        let n = t.logical();

        let all: Vec<usize> = t.cpus.iter().map(|c| c.id).collect();
        assert!(check_cpu_list(&all, &t).is_ok());

        if t.source != "none" && t.source != "available_parallelism" {
            let e = check_cpu_list(&[n + 1000], &t).unwrap_err();
            assert!(e.contains("--print-topology"), "{e}");
        }
    }
}
