#![forbid(unsafe_code)]

mod args;
mod config;
mod embedded;
mod genesis;
mod health;
mod log;
mod node;
mod paths;
mod peersdat;
mod seeds;
mod toml;
mod validator;
mod wire;

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::{Config, Network, Overrides};
use paths::Paths;
use plaine_consensus::constants as k;
use plaine_rpc::{RpcConfig, RpcServer, Shutdown};

// Distinct exit codes let an init system tell a bad config from a port clash. A
// config error is not worth restarting; a busy port might clear on its own.
const EXIT_CONFIG: u8 = 2;

const EXIT_BUSY: u8 = 3;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let action = match args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\nrun `plaine-noded --help` for usage");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    match action {
        args::Action::Help => {
            println!("{}", args::usage());
            ExitCode::SUCCESS
        }
        args::Action::Version => {
            println!("{}", args::version());

            println!("{}", env!("PLAINE_BUILD_LINE"));
            ExitCode::SUCCESS
        }
        args::Action::CheckConfig(a) => match prepare(&a, false) {
            Ok((cfg, paths)) => {
                println!("configuration is valid: {}", paths.config_file.display());
                for w in &cfg.warnings {
                    println!("warning: {w}");
                }
                ExitCode::SUCCESS
            }
            Err(code) => ExitCode::from(code),
        },
        args::Action::PrintConfig(a) => match prepare(&a, false) {
            Ok((cfg, paths)) => {
                print!("{}", cfg.print_effective(&paths));
                ExitCode::SUCCESS
            }
            Err(code) => ExitCode::from(code),
        },
        args::Action::Run(a) => match prepare(&a, a.write_config) {
            Ok((cfg, paths)) => run(cfg, paths),
            Err(code) => ExitCode::from(code),
        },
    }
}

fn prepare(a: &args::Args, write_default: bool) -> Result<(Config, Paths), u8> {
    // Two passes: resolve paths from the cli/default data dir just to find the
    // config file, then re-resolve from the data_dir the config itself may set.
    let bootstrap_dir = a.data_dir.clone().unwrap_or_else(paths::default_data_dir);
    let bootstrap = Paths::resolve(&bootstrap_dir, a.config.as_deref());

    let overrides = Overrides {
        network: None,
        data_dir: a.data_dir.clone(),
        log_level: a.log_level,
    };

    let path_text = bootstrap.config_file.display().to_string();
    let (cfg, existed) = match std::fs::read_to_string(&bootstrap.config_file) {
        Ok(src) => match config::load(&path_text, &src, &overrides) {
            Ok(c) => (c, true),
            Err(diag) => {
                eprintln!("{diag}");
                return Err(EXIT_CONFIG);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (config::defaults(&overrides), false),
        Err(e) => {
            eprintln!(
                "{}",
                toml::Diagnostic::general(&path_text, format!("cannot read the config file: {e}"))
                    .with_help(
                        "check the file's permissions, or pass --config <path> to point somewhere \
                         else"
                    )
            );
            return Err(EXIT_CONFIG);
        }
    };

    let final_paths =
        Paths::resolve(&cfg.data_dir, a.config.as_deref());

    if write_default && !existed && a.config.is_none() {
        write_default_config(&final_paths, cfg.network);
    }
    Ok((cfg, final_paths))
}

fn write_default_config(paths: &Paths, network: Network) {
    if let Err(e) = std::fs::create_dir_all(&paths.data_dir) {
        log::warn("config", format!("cannot create {}: {e}", paths.data_dir.display()));
        return;
    }
    let text = config::default_config_text(network);
    match std::fs::write(&paths.config_file, text) {
        Ok(()) => log::info(
            "config",
            format!(
                "wrote a commented default config to {} - every line in it is commented out, so \
                 it changes nothing until you edit it",
                paths.config_file.display()
            ),
        ),
        Err(e) => log::warn(
            "config",
            format!("could not write {}: {e} (continuing with defaults)", paths.config_file.display()),
        ),
    }
}

// cheap guards against a mis-built or tampered binary. a wrong header size or a
// zeroed chain id would quietly produce a chain no real network accepts.
fn self_checks() -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();

    if k::HEADER_BYTES != 132 {
        return Err("HEADER_BYTES is not 132; this is not a Plaine binary".into());
    }

    if k::CHAIN_ID == [0, 0, 0, 0] {
        warnings.push(
            "CHAIN_ID is still 0x00000000 in this build. SPEC 1 freezes it at 0x504C4E45 \
             (ASCII \"PLNE\"), and it enters every transaction signature from block 0. Do not use \
             this binary against a real network."
                .to_string(),
        );
    }

    Ok(warnings)
}

fn run(cfg: Config, paths: Paths) -> ExitCode {
    log::set_level(cfg.log_level);

    let self_check_warnings = match self_checks() {
        Ok(w) => w,
        Err(fatal) => {
            log::error("startup", fatal);
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    if let Err(e) = std::fs::create_dir_all(&paths.chain_dir) {
        log::error("startup", format!("cannot create {}: {e}", paths.chain_dir.display()));
        return ExitCode::FAILURE;
    }
    write_pid_file(&paths);

    let shutdown = Shutdown::new();
    install_signal_handler(&shutdown);

    let node = match node::start(&cfg, &paths) {
        Ok(n) => n,
        Err(e) => {
            log::error("startup", e.to_string());
            let _ = std::fs::remove_file(&paths.lock_file);
            return ExitCode::from(match e {
                node::StartError::Bind(_) => EXIT_BUSY,
                node::StartError::Genesis(_) | node::StartError::Pow(_) => EXIT_CONFIG,
                _ => 1,
            });
        }
    };

    if shutdown.is_triggered() {
        log::info("shutdown", "a stop was requested during startup; RPC will not be bound");
        let height = node.tip.height();
        node.shutdown();
        log::info("shutdown", format!("clean stop at height {height}, durable"));
        let _ = std::fs::remove_file(&paths.lock_file);
        return ExitCode::SUCCESS;
    }

    let node_views = node.rpc_views(&cfg);

    let rpc_cfg = RpcConfig {
        bind: cfg.rpc_listen,
        token: cfg.rpc_token.clone(),
        ..RpcConfig::loopback(cfg.rpc_listen.port())
    };
    let server = match RpcServer::bind(rpc_cfg, node_views, shutdown.clone()) {
        Ok(s) => s,
        Err(plaine_rpc::BindError::Io(e)) if e.kind() == std::io::ErrorKind::AddrInUse => {
            log::error(
                "rpc",
                format!(
                    "{} is already in use. Another plaine-noded is probably running against {} - \
                     one process per data directory (redb is single-writer).",
                    cfg.rpc_listen,
                    paths.chain_dir.display()
                ),
            );
            node.shutdown();
            return ExitCode::from(EXIT_BUSY);
        }
        Err(e) => {
            log::error("rpc", e.to_string());
            node.shutdown();
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    let bound = server.local_addr().unwrap_or(cfg.rpc_listen);

    banner(&cfg, &paths, bound);

    for w in self_check_warnings.iter().chain(cfg.warnings.iter()) {
        log::security("startup", w);
    }
    print_shutdown_note();

    let server = Arc::new(server);
    let serving = {
        let server = server.clone();
        std::thread::Builder::new()
            .name("plaine-rpc".into())
            .spawn(move || server.serve())
            .expect("spawning the RPC listener")
    };

    heartbeat_loop(&shutdown, &node);

    log::info("shutdown", "stopping: no new connections will be accepted");
    shutdown.trigger();
    let _ = serving.join();
    log::info("shutdown", "rpc listener stopped");

    let height = node.tip.height();
    node.shutdown();
    log::info("shutdown", format!("clean stop at height {height}, durable"));

    let _ = std::fs::remove_file(&paths.lock_file);
    log::info("shutdown", "clean exit");
    ExitCode::SUCCESS
}

fn write_pid_file(paths: &Paths) {
    let text = format!(
        "pid {}\nstarted {}\nversion {}\n{}\n",
        std::process::id(),
        log::timestamp(),
        env!("CARGO_PKG_VERSION"),

        env!("PLAINE_BUILD_LINE")
    );
    if let Ok(mut f) = std::fs::File::create(&paths.lock_file) {
        let _ = f.write_all(text.as_bytes());
    }
}

fn banner(cfg: &Config, paths: &Paths, bound: std::net::SocketAddr) {
    log::blank();
    log::info(
        "node",
        format!(
            "plaine-noded {} starting  network {}  ticker {}  chain id {}",
            env!("CARGO_PKG_VERSION"),
            cfg.network.as_str(),
            k::TICKER,

            String::from_utf8_lossy(&cfg.network.to_consensus().chain_id()).into_owned()
        ),
    );

    log::info("node", env!("PLAINE_BUILD_LINE").to_string());
    log::info("node", format!("data dir     {}", paths.chain_dir.display()));
    log::info("node", format!("config       {}", paths.config_file.display()));
    log::info(
        "node",
        format!(
            "storage      {}  txindex {}  addrindex {}",
            if cfg.prune {
                format!("pruned, keeping {} block bodies", log::thousands(k::BLOCKS_PER_YEAR))
            } else {
                "archive, keeping every block body".to_string()
            },
            if cfg.txindex { "on" } else { "off" },
            if cfg.addrindex { "on" } else { "off" }
        ),
    );

    log::debug("node", crate::health::thresholds_line());

    log::security(
        "checkpoint",
        format!(
            "{}  sunset at height {} ({} left, compiled in - config cannot extend it)",
            cfg.checkpoints.describe(),
            log::thousands(k::CHECKPOINT_SUNSET_HEIGHT),
            log::thousands(k::CHECKPOINT_SUNSET_HEIGHT),
        ),
    );
    log::security(
        "author",
        format!(
            "announcement key {} (fp {}{}), notes {} in this log",
            match cfg.author_key_source {
                plaine_rpc::views::KeySource::Embedded => "embedded in this binary",
                plaine_rpc::views::KeySource::Config => "from config",
            },
            plaine_consensus::hex::encode(&cfg.author_pubkey[..4]),
            if cfg.author_key_placeholder { ", PLACEHOLDER" } else { "" },
            if cfg.author_show_in_log { "shown" } else { "not shown" }
        ),
    );
    log::info(
        "rpc",
        format!(
            "listening on {bound} ({})",
            if plaine_rpc::server::is_loopback(&bound) {
                "loopback only, no token needed"
            } else {
                "public interface, bearer token required"
            }
        ),
    );

    log::blank();
}

fn print_shutdown_note() {
    log::info("node", "ready  (Ctrl+C for a clean stop)");
    log::debug(
        "node",
        "Ctrl+C or SIGTERM (`systemctl stop`) is a clean stop: miners are disconnected \
         with a reason, peers are closed, the validator drains, and storage is sealed and fsynced \
         - a clean stop loses no heights. A kill -9 is still safe for the database (redb leaves \
         it on a block boundary) but can lose the unsealed tail, which the node re-syncs forward.",
    );
}

fn install_signal_handler(shutdown: &Shutdown) {
    let s = shutdown.clone();
    std::thread::Builder::new()
        .name("plaine-signal".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    log::security(
                        "shutdown",
                        format!(
                            "cannot start the signal runtime ({e}); a stop will terminate the \
                             process the way the OS does by default, which can lose the unsealed \
                             tail"
                        ),
                    );
                    return;
                }
            };
            rt.block_on(async {
                let which = wait_for_stop_signal().await;
                log::info("shutdown", format!("{which} received; stopping cleanly"));
                s.trigger();
            });
        })
        .expect("spawning the signal thread");
}

#[cfg(unix)]
async fn wait_for_stop_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            log::security(
                "shutdown",
                format!(
                    "cannot register a SIGTERM handler ({e}). `systemctl stop` and `systemctl \
                     restart` will terminate this process without sealing storage, so up to {} \
                     block(s) can be lost per stop and re-synced. Ctrl+C is still clean.",
                    crate::wire::store::RING_MAX_BLOCKS
                ),
            );
            let _ = tokio::signal::ctrl_c().await;
            return "Ctrl+C (SIGINT)";
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "Ctrl+C (SIGINT)",
        _ = term.recv() => "SIGTERM",
    }
}

#[cfg(windows)]
async fn wait_for_stop_signal() -> &'static str {
    use tokio::signal::windows;

    fn unavailable(which: &str, e: std::io::Error) {
        log::security(
            "shutdown",
            format!("cannot watch for {which} ({e}); a stop delivered that way will not be clean"),
        );
    }

    let mut brk = windows::ctrl_break().map_err(|e| unavailable("Ctrl+Break", e)).ok();
    let mut close = windows::ctrl_close().map_err(|e| unavailable("the console close event", e)).ok();
    let mut down =
        windows::ctrl_shutdown().map_err(|e| unavailable("the system shutdown event", e)).ok();

    macro_rules! arm {
        ($src:expr) => {
            async {
                match $src.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            }
        };
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "Ctrl+C",
        _ = arm!(brk) => "Ctrl+Break",
        _ = arm!(close) => "the console close event",
        _ = arm!(down) => "the system shutdown event",
    }
}

fn heartbeat_loop(shutdown: &Shutdown, node: &node::Node) {
    let started = Instant::now();
    let mut tracker = health::Tracker::new(0, 0);
    let mut next_beat = Duration::from_secs(0);

    let mut sweeper = plaine_storage::sweep::HeaderSweeper::default();

    while !shutdown.is_triggered() {
        node.refresh();

        tracker.note_height(node.tip.height(), started.elapsed().as_secs());
        if started.elapsed() >= next_beat {
            let verdict = node.beat(&mut tracker, started.elapsed().as_secs());
            node.sweep_headers(&mut sweeper);
            next_beat = started.elapsed()
                + Duration::from_secs(health::heartbeat_interval(verdict.status));
        }
        // poll every 200ms so a stop is noticed quickly; only emit a heartbeat
        // when next_beat actually falls due.
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_check_passes_untampered() {
        let warnings = self_checks().expect("self-checks must pass");

        if k::CHAIN_ID == [0, 0, 0, 0] {
            assert!(warnings.iter().any(|w| w.contains("CHAIN_ID")));
        } else {
            assert!(warnings.is_empty());
        }
    }

    #[test]
    fn bad_config_never_binds() {
        let dir = std::env::temp_dir().join(format!("plaine-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cfg_path = dir.join("bad.toml");
        std::fs::write(&cfg_path, "[p2p]\nmax_peer = 5\n").expect("write");
        let a = args::Args { config: Some(cfg_path.clone()), ..Default::default() };
        assert_eq!(prepare(&a, false).err(), Some(EXIT_CONFIG));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_config_uses_embedded_keys() {
        let dir = std::env::temp_dir().join(format!("plaine-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = args::Args { data_dir: Some(dir.clone()), write_config: false, ..Default::default() };
        let (cfg, paths) = prepare(&a, false).expect("defaults must always load");
        assert!(matches!(cfg.checkpoints, config::CheckpointPolicy::Embedded { .. }));
        assert_eq!(cfg.author_pubkey, embedded::AUTHOR_KEY.bytes);
        assert!(!paths.config_file.exists(), "--check-config must not touch the disk");

        let views = plaine_rpc::mock::MockNode::synced()
            .with_network(cfg.network.to_rpc())
            .with_key_status(
                plaine_rpc::views::CheckpointStatus {
                    enabled: cfg.checkpoints.is_enabled(),
                    key_source: cfg.checkpoints.source(),
                    key_fingerprints: cfg.checkpoints.fingerprints(),
                    threshold: cfg.checkpoint_threshold,
                    last_anchor: None,
                    enforced_count: 0,
                    sunset_height: plaine_consensus::constants::CHECKPOINT_SUNSET_HEIGHT,
                    blocks_until_sunset: None,
                    sunset_passed: false,
                },
                plaine_rpc::views::AuthorKeyStatus {
                    enabled: true,
                    key_source: cfg.author_key_source,
                    fingerprint: embedded::fingerprint(&cfg.author_pubkey),
                    show_in_log: cfg.author_show_in_log,
                },
            )
            .into_node();
        assert_eq!(
            views.policy.author_key_status().fingerprint,
            embedded::AUTHOR_KEY.fingerprint()
        );
        assert_eq!(
            views.policy.checkpoint_status().key_fingerprints,
            vec![embedded::CHECKPOINT_AUTHORITY_KEY.fingerprint()]
        );

        let a = args::Args {
            data_dir: Some(dir.clone()),
            write_config: false,
            ..Default::default()
        };
        let (tcfg, _) = prepare(&a, false).expect("defaults must load");
        let tviews = plaine_rpc::mock::MockNode::synced().with_network(tcfg.network.to_rpc()).into_node();
        assert_eq!(tviews.chain.info().network, plaine_rpc::Network::Main);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_run_config_roundtrips() {
        let dir = std::env::temp_dir().join(format!("plaine-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = args::Args { data_dir: Some(dir.clone()), ..Default::default() };
        let (cfg, paths) = prepare(&a, true).expect("prepare");
        assert!(paths.config_file.exists(), "first run should write noded.toml");

        let (again, _) = prepare(&a, true).expect("second run");
        assert_eq!(again.rpc_listen, cfg.rpc_listen);
        assert_eq!(again.checkpoints, cfg.checkpoints);
        assert_eq!(again.author_pubkey, cfg.author_pubkey);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
