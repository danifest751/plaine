use plaine_pow_mine::client::args::{self, Action, Options};
use plaine_pow_mine::client::args::{batch, bench, cpu, pads, topo};
use plaine_pow_mine::client;

fn usage() -> String {
    format!(
        "plaine-miner {} - the Plaine reference CPU miner (Isochron v1, JIT)\n\
\n\
USAGE\n\
\x20 plaine-miner <POOL-URL> [options]        mine\n\
\x20 plaine-miner --bench [options]           measure this machine; no pool needed\n\
\x20 plaine-miner --print-topology            what this machine is, before you pin\n\
\x20 plaine-miner --grind <264 hex chars>     offline: one header to its own bits\n\
\n\
POOL-URL - the one required argument\n\
\x20 plne1...                       mine to this address on 127.0.0.1:9258\n\
\x20 plne1....rig1                  name the rig\n\
\x20 plne1....rig1@pool.host:9258   a remote server\n\
\x20 stratum+tcp://plne1...@host    the form pools print\n\
\n\
\x20 Solo mining pays the coinbase to the address in the URL, so it has no\n\
\x20 default. Every other option does.\n\
\n\
CPU - which processors, not just how many\n\
\x20 Thread placement matters here: a thread's 64 KiB pad lives in the core's\n\
\x20 private L2 and is mapped through an L1 dTLB it shares with its SMT sibling.\n\
\x20 Two threads on one core is a different machine from\n\
\x20 two threads on two. Run --print-topology once on a new box: sibling\n\
\x20 numbering is n/n+1 on most desktops but n/n+28 on a dual Broadwell, and a\n\
\x20 pin list built on the wrong one silently measures the wrong thing.\n\
\n\
\x20 --threads <N>          N workers, placed by the OS. Default: every processor,\n\
\x20                        capped at {} - a worker owns one of that many nonce\n\
\x20                        lanes, and workers past it repeat an earlier lane's\n\
\x20                        nonces, which a server counts as duplicate shares.\n\
\x20 --cpu-affinity <list>  Which CPUs: 0,2,4,6 or 0-3,8-11. One worker per\n\
\x20                        entry, so it already says how many - it cannot be\n\
\x20                        combined with --threads. Pinning takes effect on Linux\n\
\x20                        and Windows; macOS has no affinity API and the log\n\
\x20                        says so.\n\
\x20 --no-pin               Let the OS place the workers. By default they are\n\
\x20                        pinned, one per core first, because a worker's 64 KiB\n\
\x20                        pad lives in its core's private L2 and a thread the\n\
\x20                        scheduler moves leaves its pad behind: 25.0 kH/s pinned\n\
\x20                        against 23.3-23.7 unpinned on a Ryzen 7 8745HS.\n\
\x20 --batch <N>            Nonces per W^X seal, 1..{}. Default: as many 64 KiB\n\
\x20                        pads as half this thread's L2 share will hold, because\n\
\x20                        every pad in a batch is filled before the first one\n\
\x20                        runs. Too large and each hash starts by dragging its\n\
\x20                        pad back from L3; too small and the seal stops paying\n\
\x20                        for itself. Measured 27.1 kH/s at 4 against 24.9 at 32\n\
\x20                        on a Ryzen 7 8745HS. --bench prints what was used.\n\
\x20 --print-topology       Sockets, cores, SMT sibling numbering, performance\n\
\x20                        and efficiency cores, L2 per core, and ready-made\n\
\x20                        pin lists you can paste straight back in.\n\
\x20 --cpu-priority <0-5>   Keep a desktop usable while mining. 0 idle, 2 normal,\n\
\x20                        5 highest; nice() on Linux and macOS,\n\
\x20                        SetPriorityClass on Windows. 5 is high, not\n\
\x20                        realtime - a realtime hash loop outranks the input\n\
\x20                        thread and the machine stops responding. Default:\n\
\x20                        leave the priority where the OS put it.\n\
\x20 --huge-pages           Place scratchpads on huge pages. The median gain is\n\
\x20 --no-huge-pages        small (0.4-4%); the reason is page-colour scatter. The L2 is\n\
\x20                        physically indexed, so on 4 KiB pages a pad's page\n\
\x20                        colours are redrawn every start: between-process\n\
\x20                        spread measured 7.60% on 4 KiB and 0.12% on 2 MiB.\n\
\x20                        Default: try, fall back quietly, and report at\n\
\x20                        startup which page size was actually obtained.\n\
\n\
BENCHMARK\n\
\x20 --bench                Measure this machine. No pool, no network, no\n\
\x20                        address. Prints hash rate, cycles per hash where\n\
\x20                        this machine has a clock worth trusting, and the\n\
\x20                        conditions the figure was taken under - thread\n\
\x20                        count, which CPUs, cores actually covered, page\n\
\x20                        size obtained, and whether the clock was observed.\n\
\x20 --bench-seconds <N>    Measured window in seconds. Default {}, after a\n\
\x20                        one-second warm-up that is discarded.\n\
\n\
RUNNING\n\
\x20 --config <path>        A JSON file. Its keys are the long flags without\n\
\x20                        their `--`, the command line overrides it, and an\n\
\x20                        unknown key is an error.\n\
\x20 --status <secs>        Status line interval, 0 to silence. Default 10.\n\
\x20 --status-format <f>    text (default) or json: one JSON object per line on\n\
\x20                        stdout - status, share, block, summary - for a\n\
\x20                        program to read. Errors stay on stderr as text.\n\
\x20 --no-reconnect         Exit when the connection drops. Default: reconnect\n\
\x20                        with backoff from 1s to 30s.\n\
\x20 --max-reconnects <N>   Give up after N consecutive failed connections.\n\
\x20 --max-shares <N>       Exit after N accepted shares. Tests use this.\n\
\x20 --max-blocks <N>       Exit after N solutions met the network target.\n\
\x20 --deadline <secs>      Exit after this long whatever happened.\n\
\x20 --address <login>      The URL's login, as a flag.\n\
\x20 --stratum <h:p>        The URL's host:port, as a flag.\n\
\x20 -v, --verbose          Every line sent and received; per-thread rates and\n\
\x20                        placement under --bench.\n\
\x20 -V, --version          Print the version and exit.\n\
\x20 -h, --help             This text.\n\
\n\
OFFLINE\n\
\x20 --grind <hex>          Brute-force one 132-byte header to its own `bits`\n\
\x20                        and print the nonce. How the genesis nonce is found.\n\
\n\
CONFIG FILE\n\
\x20 {{\n\
\x20   \"url\": \"plne1youraddress.rig1@pool.example:9258\",\n\
\x20   \"cpu-affinity\": \"0-15\",\n\
\x20   \"cpu-priority\": 1,\n\
\x20   \"huge-pages\": true,\n\
\x20   \"status\": 30\n\
\x20 }}\n\
\n\
EXAMPLES\n\
\x20 plaine-miner plne1youraddress.rig1\n\
\x20 plaine-miner --print-topology\n\
\x20 plaine-miner --bench --bench-seconds 30\n\
\x20 plaine-miner --bench --cpu-affinity 0,2,4,6      one thread per core\n\
\x20 plaine-miner --bench --batch 4                   a smaller W^X batch\n\
\x20 plaine-miner plne1you.rig1 --no-pin              let the OS place workers\n\
\x20 plaine-miner plne1you.rig1@pool.example:9258 --cpu-priority 1\n\
\x20 plaine-miner --config ~/.plaine/miner.json\n",
        env!("CARGO_PKG_VERSION"),
        plaine_pow_mine::client::work::MAX_WORKERS,
        plaine_pow_mine::BATCH,
        args::DEFAULT_BENCH_SECS,
    )
}

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let mut notes: Vec<String> = Vec::new();
    let mut opts = match args::parse(&argv, &mut notes) {
        Ok(o) => o,
        Err(e) => return bad(&e),
    };

    match &opts.action {
        Action::Help => {
            print!("{}", usage());
            return std::process::ExitCode::SUCCESS;
        }
        Action::Version => {
            println!(
                "plaine-miner {} (Isochron v1, JIT, {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::ARCH
            );

            println!("{}", env!("PLAINE_BUILD_LINE"));
            return std::process::ExitCode::SUCCESS;
        }
        _ => {}
    }

    if let Some(path) = &opts.config_path {
        eprintln!("plaine-miner: settings from {path}, command line on top");
    }

    let machine = topo::Topology::detect();

    // Nonces per W^X seal: an explicit --batch, else whatever this machine's L2 will hold.
    match opts.batch {
        Some(n) => opts.client.batch = n,
        None => {
            let c = batch::auto(&machine, opts.client.threads);
            opts.client.batch = c.batch;
            if opts.client.verbose {
                notes.push(format!("batch {} - {}", c.batch, c.why));
            }
        }
    }

    if let Some(list) = opts.cpus.clone() {
        match args::check_cpu_list(&list, &machine) {
            Ok(more) => notes.extend(more),
            Err(e) => return bad(&e),
        }

        opts.client.pins = Some(cpu::resolve_pins(&list, &machine));
    } else if !opts.no_pin {
        // Default to pinning. An unpinned worker's pad is dragged between L2s every time
        // the scheduler moves it; measured at 23.3-23.7 kH/s unpinned against 25.0 pinned
        // on this machine. --no-pin restores the upstream behaviour.
        let list: Vec<usize> = machine.spread().into_iter().take(opts.client.threads).collect();
        if !list.is_empty() {
            opts.client.pins = Some(cpu::resolve_pins(&list, &machine));
            opts.cpus = Some(list);
        }
    }
    for n in &notes {
        eprintln!("plaine-miner: {n}");
    }

    if opts.action == Action::PrintTopology {
        print!("{}", machine.render());
        return std::process::ExitCode::SUCCESS;
    }

    if let Some(level) = opts.priority {
        match cpu::set_priority(level) {
            Ok(what) => eprintln!("plaine-miner: cpu priority {level} -> {what}"),
            Err(e) => eprintln!("plaine-miner: --cpu-priority {level} did not take effect: {e}"),
        }
    }

    match &opts.action {
        Action::Bench => run_bench(&opts, &machine),
        Action::Grind(hex) => {
            place_process(&opts, &machine);
            run_grind(hex, opts.client.threads)
        }
        _ => {
            report_pins(&opts);
            run_mining(&opts)
        }
    }
}

fn report_pins(opts: &Options) {
    let Some(pins) = &opts.client.pins else { return };
    eprintln!(
        "plaine-miner: cpu affinity: workers pinned, in order, to cpu {}",
        topo::fmt_list(&pins.iter().map(|(c, _)| *c).collect::<Vec<_>>())
    );
    eprintln!("plaine-miner:   (the socket thread is left unpinned on purpose)");
}

fn place_process(opts: &Options, machine: &topo::Topology) {
    let Some(list) = &opts.cpus else { return };
    let placed = cpu::restrict_process(list, machine);
    eprintln!("plaine-miner: cpu affinity: {}", placed.describe());
    if matches!(placed, cpu::Placement::Restricted(_)) {
        eprintln!(
            "plaine-miner:   (--bench pins each worker to one CPU; here the workers are \
             created inside the client and inherit the mask)"
        );
    }
}

fn report_page_request(opts: &Options) {
    match opts.pages {
        pads::Ask::Auto => {}
        pads::Ask::Force => {
            eprintln!(
                "plaine-miner: --huge-pages: requesting huge scratchpads; the startup line reports which page size was obtained"
            );
            let huge = cpu::huge_pages();
            if !huge.likely {
                eprintln!("plaine-miner:   {}", huge.state);
            }
        }
        pads::Ask::Never => {
            eprintln!("plaine-miner: --no-huge-pages: ordinary pages on purpose")
        }
    }
}

fn run_bench(opts: &Options, machine: &topo::Topology) -> std::process::ExitCode {
    let b = bench::Bench {
        secs: opts.bench_secs,
        threads: opts.client.threads,
        cpus: opts.cpus.clone(),
        pages: opts.pages,
        verbose: opts.client.verbose,
        batch: opts.client.batch,
    };
    match bench::run(&b, machine) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("plaine-miner: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_mining(opts: &Options) -> std::process::ExitCode {
    if opts.client.login.is_empty() {
        return bad(
            "give me a pool URL: `plaine-miner plne1youraddress.rig1`\n\
             solo mining pays the coinbase to that address, so it has no default",
        );
    }
    report_page_request(opts);

    eprintln!(
        "plaine-miner {}: {} threads -> {} as {}",
        env!("CARGO_PKG_VERSION"),
        opts.client.threads,
        opts.client.stratum,
        opts.client.login
    );

    let t0 = std::time::Instant::now();
    match client::run(&opts.client) {
        Ok(r) => {
            let secs = t0.elapsed().as_secs_f64().max(1e-9);
            if opts.client.status_format == client::status::Format::Json {
                let e = client::status::Event::Summary {
                    accepted: r.accepted,
                    rejected: r.rejected,
                    blocks: r.blocks,
                    hashes: r.hashes,
                    avg: r.hashes as f64 / secs,
                    uptime: secs as u64,
                };
                println!("{}", e.json());
                return std::process::ExitCode::SUCCESS;
            }
            println!(
                "plaine-miner: {} accepted, {} rejected, {} blocks, {} hashes, {} average, \
                 up {}",
                r.accepted,
                r.rejected,
                r.blocks,
                r.hashes,
                client::rate_str(r.hashes as f64 / secs),
                uptime(secs),
            );
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("plaine-miner: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn uptime(secs: f64) -> String {
    let s = secs as u64;
    match (s / 86_400, s % 86_400 / 3_600, s % 3_600 / 60, s % 60) {
        (0, 0, 0, s) => format!("{s}s"),
        (0, 0, m, s) => format!("{m}m {s}s"),
        (0, h, m, s) => format!("{h}h {m}m {s}s"),
        (d, h, m, _) => format!("{d}d {h}h {m}m"),
    }
}

fn bad(msg: &str) -> std::process::ExitCode {
    eprintln!("plaine-miner: {msg}\n\nrun `plaine-miner --help` for usage");
    std::process::ExitCode::from(2)
}

fn run_grind(hexhdr: &str, threads: usize) -> std::process::ExitCode {
    let mut header = [0u8; 132];
    if !client::json::unhex(hexhdr.trim(), &mut header) {
        return bad("--grind needs exactly 264 hex characters (132 bytes)");
    }
    let bits = u32::from_le_bytes([header[116], header[117], header[118], header[119]]);
    let target = match plaine_consensus::asert::Target::from_compact(bits) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("plaine-miner: header bits 0x{bits:08x} do not decode: {e:?}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut target_be = [0u8; 32];
    for (i, limb) in target.0.iter().enumerate() {
        let be = limb.to_be_bytes();
        let off = 24 - i * 8;
        target_be[off..off + 8].copy_from_slice(&be);
    }
    eprintln!(
        "plaine-miner: grinding bits 0x{bits:08x} target {} on {threads} threads",
        client::json::hex(&target_be)
    );
    let t0 = std::time::Instant::now();
    match client::grind(&header, &target_be, threads, 0) {
        Some((nonce, ph, hashes)) => {
            let secs = t0.elapsed().as_secs_f64();
            eprintln!(
                "plaine-miner: {hashes} hashes in {secs:.1}s ({:.0} H/s)",
                hashes as f64 / secs.max(1e-9)
            );
            println!("nonce    {nonce}");
            println!("pow_hash {}", client::json::hex(&ph));
            std::process::ExitCode::SUCCESS
        }
        None => {
            eprintln!("plaine-miner: grind failed");
            std::process::ExitCode::FAILURE
        }
    }
}
