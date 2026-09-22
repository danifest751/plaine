use super::cpu::{self, Placement};
use super::pads::Ask;
use super::topo::{self, Topology};
use crate::client::rate_str;
use crate::pad::{PadPages, Summary};
use crate::{Miner, Pads, BATCH};
use plaine_consensus::pow;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WARMING: u8 = 0;

const MEASURING: u8 = 1;

const STOPPED: u8 = 2;

#[derive(Debug, Clone)]
pub struct Bench {
    pub secs: u64,
    pub threads: usize,
    pub cpus: Option<Vec<usize>>,
    pub pages: Ask,
    pub verbose: bool,
}

struct Worker {
    hashes: u64,
    secs: f64,
    cycles: Option<u64>,
    placement: Option<Placement>,
    pages: PadPages,
    cpu: Option<usize>,
}

pub fn run(b: &Bench, topo: &Topology) -> Result<(), String> {
    measure(b, topo).map(|_| ())
}

fn measure(b: &Bench, topo: &Topology) -> Result<Summary, String> {
    if b.secs == 0 {
        return Err("--bench-seconds needs at least 1".into());
    }
    let threads = b.threads.max(1);

    let warmup = if b.secs >= 5 { 1.0f64 } else { 0.5 };

    // A hash rate measured on a JIT that does not compute Isochron is a number for
    // nothing, so the benchmark takes the same gate the mining path does.
    crate::client::preflight_verbose(b.verbose).map_err(|e| e.to_string())?;

    println!("plaine-miner --bench");
    println!(
        "  Isochron v1 - 64 KiB scratchpad, {BATCH} nonces per W^X seal, JIT\n"
    );
    if cfg!(debug_assertions) {
        println!(
            "  This is a debug build; the number below is not this miner's speed.\n\
             \x20 Build with --release before quoting anything from this run.\n"
        );
    }
    println!(
        "  warm-up {warmup:.1} s, then measuring {} s on {threads} thread{}",
        b.secs,
        if threads == 1 { "" } else { "s" }
    );

    let phase = Arc::new(AtomicU8::new(WARMING));
    let counters: Arc<Vec<AtomicU64>> =
        Arc::new((0..threads).map(|_| AtomicU64::new(0)).collect());

    let mut handles = Vec::with_capacity(threads);
    for i in 0..threads {
        let pin = b
            .cpus
            .as_ref()
            .and_then(|v| cpu::resolve_pins(v, topo).get(i).copied());
        let phase = phase.clone();
        let counters = counters.clone();
        let ask = b.pages;
        handles.push(
            std::thread::Builder::new()
                .name(format!("plaine-bench-{i}"))
                .spawn(move || worker(i, pin, ask, phase, counters))
                .map_err(|e| format!("cannot spawn benchmark worker {i}: {e}"))?,
        );
    }

    std::thread::sleep(Duration::from_secs_f64(warmup));

    let probe = super::clock::Probe::start(&probe_cpus(b, topo));
    let t0 = Instant::now();
    phase.store(MEASURING, Ordering::SeqCst);

    let mut last = (0u64, t0);
    while t0.elapsed() < Duration::from_secs(b.secs) {
        std::thread::sleep(Duration::from_millis(250));
        let now = Instant::now();
        if now.duration_since(last.1) < Duration::from_millis(950) {
            continue;
        }
        let total: u64 = counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
        let dt = now.duration_since(last.1).as_secs_f64();
        println!("    {:>5.1} s   {}", t0.elapsed().as_secs_f64(), rate_str((total - last.0) as f64 / dt));
        last = (total, now);
    }
    let window = t0.elapsed().as_secs_f64();
    phase.store(STOPPED, Ordering::SeqCst);

    let mut workers: Vec<Worker> = Vec::with_capacity(threads);
    for h in handles {
        match h.join() {
            Ok(Ok(w)) => workers.push(w),
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("a benchmark worker panicked".into()),
        }
    }
    let observed = probe.finish(
        &workers.iter().map(|w| w.cycles).collect::<Vec<_>>(),
        window,
    );

    Ok(report(b, topo, &workers, window, &observed))
}

fn worker(
    index: usize,
    pin: Option<(usize, u16)>,
    ask: Ask,
    phase: Arc<AtomicU8>,
    counters: Arc<Vec<AtomicU64>>,
) -> Result<Worker, String> {
    let placement = pin.map(|(c, g)| cpu::pin_current_thread(c, g));
    let mut miner = Miner::new().map_err(|e| format!("benchmark worker {index}: {e:?}"))?;

    let mut pads = Pads::new(BATCH, ask.wanted())
        .map_err(|e| format!("benchmark worker {index} cannot map scratchpads: {e}"))?;
    let pages = pads.pages();
    let counter = super::clock::ThreadCounter::open();

    let header = [0u8; 132];
    let mut seeds = [0u64; BATCH];
    let mut digests = [0u64; BATCH];
    let mut headers = [[0u8; 132]; BATCH];
    let mut counter_value: u64 = 0;

    let mut sink: u64 = 0;

    let mut measuring = false;
    let mut local = 0u64;
    let mut cycles_at_start: Option<u64> = None;
    let mut started = Instant::now();

    loop {
        match phase.load(Ordering::Relaxed) {
            STOPPED => break,
            MEASURING if !measuring => {
                measuring = true;
                cycles_at_start = counter.as_ref().and_then(|c| c.read());
                started = Instant::now();
            }
            _ => {}
        }
        for s in 0..BATCH {
            let nonce = ((index as u64) << 40) | counter_value;
            counter_value = counter_value.wrapping_add(1);
            headers[s] = header;
            headers[s][124..].copy_from_slice(&nonce.to_le_bytes());
            seeds[s] = pow::seed(&headers[s]);
        }
        miner
            .mine_hash_batch_on(&mut pads, &seeds, &mut digests)
            .map_err(|e| format!("benchmark worker {index}: JIT failure {e:?}"))?;
        for s in 0..BATCH {
            let h = pow::pow_hash(&headers[s], digests[s]);
            sink ^= u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]]);
        }

        sink = std::hint::black_box(sink);
        if measuring {
            local += BATCH as u64;
            counters[index].store(local, Ordering::Relaxed);
        }
    }
    let secs = started.elapsed().as_secs_f64();
    std::hint::black_box(sink);
    let cycles = match (cycles_at_start, counter.as_ref().and_then(|c| c.read())) {
        (Some(a), Some(b)) if b >= a => Some(b - a),
        _ => None,
    };
    Ok(Worker {
        hashes: local,
        secs,
        cycles,
        placement,
        pages,
        cpu: pin.map(|(c, _)| c),
    })
}

fn probe_cpus(b: &Bench, topo: &Topology) -> Vec<usize> {
    match &b.cpus {
        Some(list) => list.clone(),
        None if b.threads.max(1) >= topo.logical() => topo.cpus.iter().map(|c| c.id).collect(),
        None => Vec::new(),
    }
}

fn unpinned_clock_note(b: &Bench, topo: &Topology) -> Option<String> {
    (b.cpus.is_none() && b.threads.max(1) < topo.logical()).then(|| {
        format!(
            "{} workers were left unpinned on a {}-processor machine, so which processors' \
             cycles belong to this run is not knowable; --cpu-affinity gives a cycles figure",
            b.threads.max(1),
            topo.logical()
        )
    })
}

fn page_line(ask: Ask, s: Summary, base_page: usize) -> String {
    let got = if s.mixed() {
        format!("{} pages", s.tag())
    } else {
        ask.describe(s.kind())
    };
    match (s.huge(), s.total()) {
        (0, _) => format!("{got}, base page {} KiB", base_page / 1024),
        (h, t) if h == t => format!("{got} for all {t} worker regions"),
        (h, t) => format!(
            "{got} for {h} of {t} worker regions, the rest on ordinary pages - this run mixes \
             the two, so its rate is not either arm's"
        ),
    }
}

fn report(
    b: &Bench,
    topo: &Topology,
    w: &[Worker],
    window: f64,
    observed: &super::clock::Observed,
) -> Summary {
    let hashes: u64 = w.iter().map(|x| x.hashes).sum();

    let rates: Vec<f64> =
        w.iter().map(|x| x.hashes as f64 / x.secs.max(1e-9)).collect();
    let total: f64 = rates.iter().sum();
    let (mut lo, mut hi) = (f64::MAX, 0f64);
    for r in &rates {
        lo = lo.min(*r);
        hi = hi.max(*r);
    }

    println!("\n  RESULT");
    println!(
        "    hash rate      {:<18} {} per thread{}",
        rate_str(total),
        rate_str(total / w.len().max(1) as f64),
        if w.len() > 1 {
            format!(" (slowest {}, fastest {})", rate_str(lo), rate_str(hi))
        } else {
            String::new()
        }
    );
    match observed.per_hash(hashes) {
        Some(cph) => {
            let how = match observed {
                super::clock::Observed::Cycles { how, .. } => how.clone(),
                super::clock::Observed::Absent { .. } => String::new(),
            };
            println!("    cycles/hash    {:<18} {how}", thousands(cph as u64));
        }
        None => {
            let why = match observed {
                super::clock::Observed::Absent { why } => why.clone(),
                super::clock::Observed::Cycles { .. } => "no hashes were counted".into(),
            };
            println!("    cycles/hash    not measured");
            println!("                   {why}");
            if let Some(note) = unpinned_clock_note(b, topo) {
                println!("                   {note}");
            }
            if let Some(true) = super::clock::invariant_tsc() {
                println!(
                    "                   (this part does have an invariant TSC, but that is a\n\
                     \x20                   reference clock - it ticks at a fixed rate while the\n\
                     \x20                   core's does not, so it cannot give cycles per hash)"
                );
            }
        }
    }
    println!("    hashes         {:<18} in {window:.1} s", thousands(hashes));

    println!("\n  CONDITIONS");
    println!("    threads        {}", w.len());

    match &b.cpus {
        Some(list) => {
            let pinned = w.iter().filter(|x| x.placement.as_ref().is_some_and(|p| p.is_pinned())).count();
            println!("    CPUs           {} (asked for)", topo::fmt_list(list));
            if pinned == w.len() {
                println!("                   every worker pinned to its own CPU");
            } else {
                println!(
                    "                   {pinned} of {} workers actually pinned:",
                    w.len()
                );
                for x in w {
                    if let (Some(c), Some(p)) = (x.cpu, x.placement.as_ref()) {
                        if !p.is_pinned() {
                            println!("                     CPU {c}: {}", p.describe());
                        }
                    }
                }
            }
        }
        None => {
            let allowed = cpu::allowed_cpus();
            match &allowed {
                Some(v) if v.len() < topo.logical() => println!(
                    "    CPUs           not pinned; this process is allowed only {} of the {} \
                     the machine has",
                    topo::fmt_list(v),
                    topo.logical()
                ),
                _ => println!(
                    "    CPUs           not pinned - the OS placed the workers, and it may \
                     have moved them"
                ),
            }
        }
    }
    if let Some(list) = &b.cpus {
        match topo.cores_covered(list) {
            Some(n) => {
                let cores = topo.cores().map(|c| c.len()).unwrap_or(0);
                println!(
                    "    cores covered  {n} physical core{} out of {cores}{}",
                    if n == 1 { "" } else { "s" },
                    if n < list.len() {
                        format!(" - {} of the {} CPUs are SMT siblings sharing a core", list.len() - n, list.len())
                    } else {
                        String::new()
                    }
                );
            }
            None => println!("    cores covered  unknown: this OS did not report core identity"),
        }
        if topo.hybrid() {
            let classes = topo.classes();
            let counts: Vec<String> = classes
                .iter()
                .rev()
                .map(|c| {
                    let n = list
                        .iter()
                        .filter(|id| topo.cpu(**id).and_then(|x| x.class) == Some(*c))
                        .count();
                    format!("{n} x {}", topo.class_label(Some(*c)))
                })
                .collect();
            println!("    core types     {}", counts.join(", "));
            if counts.len() > 1 && !counts.iter().any(|c| c.starts_with("0 ")) {
                println!(
                    "                   this run mixed performance and efficiency cores, which\n\
                     \x20                  differ by about 28% per clock at this pad size, so the\n\
                     \x20                  figure above is a machine total and not a per-core rate"
                );
            }
        }
    }

    let pages = Summary::of(w.iter().map(|x| x.pages));
    println!("    page size      {}", page_line(b.pages, pages, cpu::page_size()));
    if pages.huge() < pages.total() {
        // Prefer what the allocator actually reported. huge_pages().state is a static
        // explanation that names the privilege, and on a machine that holds it and
        // failed for another reason that line sends the reader somewhere useless.
        match crate::pad::last_note() {
            Some(note) => println!("                   {note}"),
            None => println!("                   {}", cpu::huge_pages().state),
        }
    }
    println!(
        "    scratchpad     64 KiB per nonce x {BATCH} in flight = {} MiB per thread, \
         {} MiB total",
        BATCH * 64 / 1024,
        w.len() * BATCH * 64 / 1024
    );
    match observed {
        super::clock::Observed::Cycles { how, .. } => println!("    clock          {how}"),
        super::clock::Observed::Absent { why } => println!("    clock          not observed - {why}"),
    }
    if !topo.model.is_empty() {
        println!("    machine        {}", topo.model);
    }
    println!(
        "                   {} logical processors{}{}",
        topo.logical(),
        match topo.cores() {
            Some(c) => format!(", {} physical cores", c.len()),
            None => String::new(),
        },
        match topo.packages() {
            Some(p) if p > 1 => format!(", {p} sockets"),
            _ => String::new(),
        }
    );
    println!(
        "    build          {} {}",
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "debug build - this is not the miner's speed"
        } else {
            "release"
        }
    );

    if b.verbose {
        println!("\n  PER THREAD");
        println!("    worker  CPU      hash rate      pages       placement");
        for (i, x) in w.iter().enumerate() {
            println!(
                "    {:<7} {:<8} {:<14} {:<11} {}",
                i,
                x.cpu.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                rate_str(x.hashes as f64 / x.secs.max(1e-9)),
                x.pages.tag(),
                x.placement.as_ref().map(|p| p.describe()).unwrap_or_else(|| {
                    "not pinned (--threads places nothing; use --cpu-affinity)".into()
                })
            );
        }
    }
    println!();
    pages
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1 000");
        assert_eq!(thousands(2_480_137), "2 480 137");
    }

    #[test]
    fn zero_window_is_refused() {
        let b = Bench { secs: 0, threads: 1, cpus: None, pages: Ask::Auto, verbose: false };
        assert!(run(&b, &Topology::detect()).is_err());
    }

    #[test]
    fn one_thread_produces_a_rate() {
        let b = Bench { secs: 1, threads: 1, cpus: None, pages: Ask::Auto, verbose: true };
        run(&b, &Topology::detect()).expect("the benchmark must run on its own machine");
    }

    #[test]
    fn conditions_count_regions_after_join() {
        let threads = 2;
        let b = Bench { secs: 1, threads, cpus: None, pages: Ask::Auto, verbose: false };
        let topo = Topology::detect();

        let (held, err) = Pads::many(3, BATCH, false);
        assert!(err.is_none(), "the ordinary-page path must never fail: {err:?}");
        assert_eq!(held.len(), 3);

        for run in 1..=2 {
            let pages = measure(&b, &topo).expect("the benchmark must run on its own machine");
            assert_eq!(
                pages.total(),
                threads,
                "benchmark run {run} reported {} worker regions for a {threads}-thread run",
                pages.total()
            );
            assert!(pages.huge() <= pages.total());

            let line = page_line(b.pages, pages, 4096);
            assert!(
                pages.huge() == 0 || line.contains(&format!("{threads} worker regions")),
                "the page line lost the region count: {line}"
            );
        }
        drop(held);
    }

    #[test]
    fn page_line_shows_every_region() {
        let none = Summary::of(std::iter::repeat_n(PadPages::Base, 12));
        let all = Summary::of(std::iter::repeat_n(PadPages::HugeTlb, 12));
        let some = Summary::of(
            std::iter::repeat_n(PadPages::Large, 4).chain(std::iter::repeat_n(PadPages::Base, 8)),
        );
        let two_ways = Summary::of(
            std::iter::repeat_n(PadPages::HugeTlb, 1)
                .chain(std::iter::repeat_n(PadPages::Transparent, 11)),
        );

        assert_eq!(
            page_line(Ask::Auto, none, 4096),
            "ordinary pages, base page 4 KiB"
        );

        assert!(page_line(Ask::Force, none, 4096).contains("did not get them"));

        assert_eq!(
            page_line(Ask::Auto, all, 4096),
            "2M/hugetlb pages for all 12 worker regions"
        );

        let partial = page_line(Ask::Auto, some, 4096);
        assert!(partial.contains("4 of 12 worker regions"), "{partial}");
        assert!(partial.contains("this run mixes"), "{partial}");
        assert!(!partial.contains("all 12"), "a partial success reported as complete: {partial}");

        assert_eq!(
            page_line(Ask::Auto, two_ways, 4096),
            "2M/mixed pages for all 12 worker regions"
        );
    }
}
