pub mod args;
pub mod json;
pub mod work;

use crate::{pad, Miner, Pads, BATCH};
use plaine_consensus::pow;
use json::Framed;
use std::collections::HashSet;
use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use work::{JobView, Shared, Solution};

pub const BACKOFF_MIN: Duration = Duration::from_secs(1);

pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub const MAX_LINE: usize = 8 * 1024;

pub const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

pub const AUTH_BACKOFF_MIN: Duration = Duration::from_secs(150);

pub const AUTH_BACKOFF_MAX: Duration = Duration::from_secs(600);

pub const SERVER_SILENCE_DEADLINE: Duration = Duration::from_secs(90);

#[derive(Debug, Clone)]
pub struct Args {
    pub stratum: String,
    pub login: String,
    pub threads: usize,
    pub max_shares: u64,
    pub max_blocks: u64,
    pub deadline_secs: u64,
    pub verbose: bool,
    pub reconnect: bool,
    pub max_reconnects: u32,
    pub status_secs: u64,
    pub silence_deadline_ms: u64,
    pub huge_pages: bool,
    pub pins: Option<Vec<(usize, u16)>>,
}

impl Default for Args {
    fn default() -> Args {
        Args {
            stratum: format!("{}:{}", args::DEFAULT_HOST, args::DEFAULT_PORT),
            login: String::new(),
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            max_shares: 0,
            max_blocks: 0,
            deadline_secs: 0,
            verbose: false,
            reconnect: true,
            max_reconnects: 0,
            status_secs: 10,
            silence_deadline_ms: SERVER_SILENCE_DEADLINE.as_millis() as u64,
            huge_pages: true,
            pins: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    pub accepted: u64,
    pub rejected: u64,
    pub blocks: u64,
    pub hashes: u64,
    pub connections: u64,
    pub stale_dropped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ladder {
    Connection,
    Authorization,
}

#[derive(Debug)]
pub enum Ended {
    Limit,
    ServerClosed,
    Retry(Ladder, String),
    NoWorkers(String),
}

#[derive(Debug, Clone, Copy)]
struct Rungs {
    min: Duration,
    max: Duration,
    wait: Duration,
}

impl Rungs {
    const fn new(min: Duration, max: Duration) -> Rungs {
        Rungs { min, max, wait: min }
    }

    fn step(&mut self) -> Duration {
        let now = self.wait;
        self.wait = saturating_double(self.wait, self.max);
        now
    }

    fn reset(&mut self) {
        self.wait = self.min;
    }
}

fn saturating_double(d: Duration, cap: Duration) -> Duration {
    d.saturating_mul(2).min(cap)
}

fn say_once(last: &mut Option<String>, why: &str) -> bool {
    if last.as_deref() == Some(why) {
        return false;
    }
    eprintln!("plaine-miner: {why}");
    *last = Some(why.to_string());
    true
}

pub fn preflight() -> std::io::Result<()> {
    if let Err(e) = Miner::new() {
        return Err(std::io::Error::other(format!(
            "this CPU cannot run Isochron: {e:?}. The algorithm needs hardware AES \
             (AES-NI on x86-64, the ARMv8 crypto extensions on aarch64), which this \
             machine does not have. Nothing was mined."
        )));
    }
    Ok(())
}

fn all_workers_stopped(workers: &[std::thread::JoinHandle<()>]) -> bool {
    !workers.is_empty() && workers.iter().all(|h| h.is_finished())
}

fn no_workers_is_fatal(totals: &Report) -> bool {
    totals.hashes == 0
}

pub fn run(args: &Args) -> std::io::Result<Report> {
    preflight()?;

    let start = Instant::now();
    let mut totals = Report::default();

    let mut conn = Rungs::new(BACKOFF_MIN, BACKOFF_MAX);
    let mut auth = Rungs::new(AUTH_BACKOFF_MIN, AUTH_BACKOFF_MAX);
    let mut failures = 0u32;

    let mut said: Option<String> = None;

    loop {
        let attempt_started = Instant::now();
        let before = totals.connections;
        let outcome = session(args, start, &mut totals);
        let lasted = attempt_started.elapsed();

        let authorized = totals.connections > before;

        let ladder = match outcome {
            Ok(Ended::Limit) => return Ok(totals),

            Ok(Ended::NoWorkers(why)) if no_workers_is_fatal(&totals) => {
                return Err(std::io::Error::other(format!("{why}. Nothing was mined.")));
            }
            Ok(Ended::NoWorkers(why)) => {
                let _ = say_once(&mut said, &why);
                Ladder::Connection
            }
            Ok(Ended::Retry(ladder, why)) => {
                let _ = say_once(&mut said, &why);
                ladder
            }
            Ok(Ended::ServerClosed) => {
                let _ = say_once(&mut said, "server closed the connection");
                Ladder::Connection
            }
            Err(e) => {
                if !args.reconnect {
                    eprintln!("plaine-miner: {}: {e}", args.stratum);
                    return Err(e);
                }
                let _ = say_once(&mut said, &format!("{}: {e}", args.stratum));
                Ladder::Connection
            }
        };

        if !args.reconnect || limit_reached(args, start, &totals) {
            return Ok(totals);
        }

        if lasted >= BACKOFF_RESET_AFTER {
            conn.reset();
            failures = 0;
            said = None;
        }

        if authorized {
            auth.reset();
            said = None;
        }
        failures += 1;
        if args.max_reconnects > 0 && failures > args.max_reconnects {
            eprintln!("plaine-miner: giving up after {failures} failed connections");
            return Ok(totals);
        }

        let step = match ladder {
            Ladder::Connection => conn.step(),
            Ladder::Authorization => auth.step(),
        };
        let wait = match remaining(args, start) {
            Some(left) if left < step => left,
            _ => step,
        };

        let verb = match ladder {
            Ladder::Connection => "reconnecting to",
            Ladder::Authorization => "retrying authorization at",
        };
        eprintln!(
            "plaine-miner: {verb} {} in {:.1}s (attempt {})",
            args.stratum,
            wait.as_secs_f64(),
            failures + 1
        );
        std::thread::sleep(wait);

        if limit_reached(args, start, &totals) {
            return Ok(totals);
        }
    }
}

fn remaining(args: &Args, start: Instant) -> Option<Duration> {
    if args.deadline_secs == 0 {
        return None;
    }
    Some(Duration::from_secs(args.deadline_secs).saturating_sub(start.elapsed()))
}

fn limit_reached(args: &Args, start: Instant, r: &Report) -> bool {
    if args.max_shares > 0 && r.accepted >= args.max_shares {
        return true;
    }
    if args.max_blocks > 0 && r.blocks >= args.max_blocks {
        return true;
    }
    matches!(remaining(args, start), Some(d) if d.is_zero())
}

pub fn session(args: &Args, start: Instant, totals: &mut Report) -> std::io::Result<Ended> {
    let stream = TcpStream::connect(&args.stratum)?;
    stream.set_nodelay(true)?;

    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut w = stream.try_clone()?;

    let mut r = json::Lines::new(stream, MAX_LINE);

    let shared = Arc::new(Shared::new(args.threads.max(1)));
    let (tx, rx) = mpsc::channel::<Solution>();

    let mut next_id = 1u64;
    let subscribe_id = next_id;
    send(
        &mut w,
        args,
        &format!(
            "{{\"id\":{next_id},\"method\":\"mining.subscribe\",\"params\":[\"plaine-miner/0.1\"]}}"
        ),
    )?;
    next_id += 1;

    let mut authorize_id = 0u64;
    let mut submits: HashSet<u64> = HashSet::new();
    let mut authorized = false;
    let mut subscribed = false;
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut jobs = JobTrack::default();
    let mut status = Status::new(args.status_secs, start, args.verbose);

    let login_json = json_escape(&args.login);

    let silence = Duration::from_millis(args.silence_deadline_ms);
    let probe_every = silence / 3; // three keepalives fit inside one silence window
    let mut last_line_at = Instant::now();

    let mut last_out_at = Instant::now();

    let ended = loop {
        if limit_reached(args, start, totals) {
            break Ended::Limit;
        }
        if all_workers_stopped(&workers) {
            break Ended::NoWorkers(format!(
                "all {} workers have stopped; this connection is not hashing",
                workers.len()
            ));
        }

        while let Ok(sol) = rx.try_recv() {
            if !jobs.submittable(sol.job_id) {
                totals.stale_dropped += 1;
                if args.verbose {
                    eprintln!("plaine-miner: dropping a solution for retired job {:08x}", sol.job_id);
                }
                continue;
            }
            if sol.is_block {
                totals.blocks += 1;
                println!(
                    "plaine-miner: block candidate at height {} - {}",
                    sol.height,
                    json::hex(&sol.pow_hash)
                );
            }
            // Echo the server's own spelling of the job id, never a reformatted one.
            // The fallback cannot normally fire: submittable() just said this job is
            // live or inside its grace, and both keep the string.
            let fallback;
            let job_id_hex = match jobs.hex_of(sol.job_id) {
                Some(h) => h,
                None => {
                    fallback = format!("{:08x}", sol.job_id);
                    &fallback
                }
            };
            let msg = format!(
                "{{\"id\":{next_id},\"method\":\"mining.submit\",\"params\":[{login_json},\"{job_id_hex}\",\"{}\"]}}",
                work::nonce_hex(sol.nonce)
            );
            submits.insert(next_id);
            next_id += 1;
            send(&mut w, args, &msg)?;
            last_out_at = Instant::now();
        }

        status.maybe_print(&shared, totals, &jobs);

        if !silence.is_zero() {
            let quiet = last_line_at.elapsed();
            if quiet >= silence {
                break Ended::Retry(
                    Ladder::Connection,
                    format!(
                        "the server has been silent for {}s and did not answer \
                         mining.keepalive; this wire pushes a job every {}s, so the \
                         connection is gone. Reconnecting",
                        quiet.as_secs(),
                        work::SERVER_JOB_REFRESH.as_secs(),
                    ),
                );
            }

            if last_out_at.elapsed() >= probe_every {
                send(
                    &mut w,
                    args,
                    &format!("{{\"id\":{next_id},\"method\":\"mining.keepalive\"}}"),
                )?;
                next_id += 1;
                last_out_at = Instant::now();
            }
        }

        let framed = match r.next() {
            Ok(f) => f,

            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                break Ended::ServerClosed
            }
            Err(e) => return Err(e),
        };
        let raw = match framed {
            Framed::Line(l) => {
                last_line_at = Instant::now();
                l
            }
            Framed::Idle => continue,
            Framed::Eof => break Ended::ServerClosed,

            Framed::TooLong => {
                break Ended::Retry(
                    Ladder::Connection,
                    format!(
                        "server sent a line longer than {MAX_LINE} bytes; this is not a \
                         stratum server. Check the port in the pool URL - retrying"
                    ),
                )
            }
        };
        let Ok(text) = core::str::from_utf8(&raw) else {
            continue;
        };
        if args.verbose {
            eprintln!("<< {text}");
        }
        let Some(msg) = json::parse(text) else {
            continue;
        };

        match msg.method.as_deref() {
            Some("mining.notify") => {
                let (Some(job_hex), Some(height), Some(prefix_hex)) =
                    (msg.str_at(0), msg.num_at(1), msg.str_at(2))
                else {
                    continue;
                };
                let mut prefix = [0u8; work::PREFIX_BYTES];
                if !json::unhex(prefix_hex, &mut prefix) {
                    eprintln!("plaine-miner: notify prefix is not 248 hex characters");
                    continue;
                }
                let Ok(job_id) = u32::from_str_radix(job_hex, 16) else {
                    continue;
                };
                let clean = msg.bool_at(3).unwrap_or(false);
                let target = shared.job().map(|j| j.target).unwrap_or([0xff; 32]);
                jobs.notify(job_id, job_hex, clean);
                shared.publish(JobView {
                    job_id,
                    job_id_hex: job_hex.to_string(),
                    prefix,
                    target,
                    network_target: work::network_target_from_prefix(&prefix).unwrap_or([0u8; 32]),
                    height,
                    live: true,
                });
                if args.verbose {
                    eprintln!(
                        "plaine-miner: job {job_hex} height {height}{}",
                        if clean { " (clean)" } else { "" }
                    );
                }
            }
            Some("mining.set_target") => {
                let Some(t) = msg.str_at(0) else { continue };
                let mut target = [0u8; 32];
                if !json::unhex(t, &mut target) {
                    eprintln!("plaine-miner: set_target is not 64 hex characters");
                    continue;
                }
                retarget(&shared, target);
            }

            Some("mining.set_difficulty") => {
                let Some(d) = msg.num_at(0) else { continue };
                eprintln!(
                    "plaine-miner: server sent mining.set_difficulty {d}; this wire uses \
                     mining.set_target, converting"
                );
                retarget(&shared, work::target_from_difficulty(d));
            }
            Some("client.reconnect") => {
                eprintln!("plaine-miner: ignoring client.reconnect (hijack vector)");
            }
            Some(_) => {}
            None => {
                if msg.id == Some(subscribe_id) && !subscribed {
                    let Some(e1_hex) = msg.str_at(1) else {
                        break Ended::Retry(
                            Ladder::Connection,
                            "subscribe response has no extranonce1; the server is not \
                             assigning nonce slices"
                                .into(),
                        );
                    };
                    let Ok(e1) = u32::from_str_radix(e1_hex, 16) else {
                        break Ended::Retry(
                            Ladder::Connection,
                            format!("extranonce1 {e1_hex:?} is not hex"),
                        );
                    };

                    let x_bytes = msg.num_at(2).unwrap_or(work::X_BYTES);
                    if !(work::X_BYTES_MIN..=work::X_BYTES).contains(&x_bytes) {
                        break Ended::Retry(
                            Ladder::Connection,
                            format!(
                                "server says {x_bytes} rollable nonce bytes; this miner rolls \
                                 {}..={}. Every share would be refused with error 25. This is \
                                 a server or proxy misconfiguration; retrying in case it \
                                 is corrected",
                                work::X_BYTES_MIN,
                                work::X_BYTES
                            ),
                        );
                    }
                    let x_bits = (x_bytes * 8) as u32;

                    if (e1 as u64) >= work::e1_limit(x_bits) {
                        break Ended::Retry(
                            Ladder::Connection,
                            format!(
                                "server assigned extranonce1 {e1_hex} with a {x_bytes}-byte \
                                 rollable window; that leaves {} bits for the slice, fewer \
                                 than this miner needs",
                                64 - x_bits
                            ),
                        );
                    }
                    shared.set_window(e1, x_bits);
                    if x_bits != work::X_BITS {
                        eprintln!(
                            "plaine-miner: server assigned a narrower nonce window: {x_bytes} \
                             rollable bytes, slice {e1_hex} - a proxy is subdividing its \
                             upstream slice, and this rig has {} nonces per worker",
                            1u64 << (x_bits - work::THREAD_BITS)
                        );
                    }
                    subscribed = true;
                    authorize_id = next_id;
                    send(
                        &mut w,
                        args,
                        &format!(
                            "{{\"id\":{next_id},\"method\":\"mining.authorize\",\"params\":[{login_json},\"x\"]}}"
                        ),
                    )?;
                    next_id += 1;
                    last_out_at = Instant::now();
                } else if msg.id == Some(authorize_id) && !authorized {
                    if msg.is_error || msg.result_false || !(msg.result_true || msg.result_object) {
                        break Ended::Retry(
                            Ladder::Authorization,
                            format!(
                                "authorize refused: {} {} - check the address in the login. \
                                 Retrying on the slow ladder ({}s and up, to {}s), because a \
                                 refused login re-offered quickly gets this IP banned",
                                msg.error_code.unwrap_or(0),
                                msg.error_msg.as_deref().unwrap_or(""),
                                AUTH_BACKOFF_MIN.as_secs(),
                                AUTH_BACKOFF_MAX.as_secs(),
                            ),
                        );
                    }
                    authorized = true;
                    totals.connections += 1;

                    eprintln!(
                        "plaine-miner: authorized as {}  ({} threads)",
                        args.login, args.threads
                    );
                    if args.verbose {
                        eprintln!(
                            "plaine-miner: extranonce1 {:0w$x} ({} rollable bytes)",
                            shared.e1(),
                            shared.x_bits() / 8,
                            w = ((64 - shared.x_bits()) / 4) as usize
                        );
                    }

                    let (pads, err) = Pads::many(args.threads.max(1), BATCH, args.huge_pages);
                    if let Some(e) = err {
                        if pads.is_empty() {
                            break Ended::NoWorkers(format!(
                                "not one scratchpad region could be mapped, so no worker can \
                                 start: {e}"
                            ));
                        }
                        eprintln!(
                            "plaine-miner: cannot map scratchpads for worker {}: {e}; \
                             continuing with {} workers",
                            pads.len(),
                            pads.len()
                        );
                    }
                    for (i, p) in pads.into_iter().enumerate() {
                        workers.push(spawn_worker(
                            i,
                            shared.clone(),
                            tx.clone(),
                            p,
                            args.pins.as_ref().and_then(|q| q.get(i).copied()),
                        ));
                    }
                    pad::log_startup(args.verbose);
                } else if submits.remove(&msg.id.unwrap_or(u64::MAX)) {
                    // Accepted unless the server actually said no. Stratum v1 spells yes
                    // as `result:true`, as `result:{"status":"OK"}` (rplant.xyz), and as
                    // a bare `error:null`; only an error object or a literal `false` is
                    // a refusal.
                    if msg.is_error || msg.result_false {
                        totals.rejected += 1;
                        eprintln!(
                            "plaine-miner: share rejected {} {}",
                            msg.error_code.unwrap_or(0),
                            msg.error_msg.as_deref().unwrap_or("")
                        );
                    } else {
                        totals.accepted += 1;
                        println!(
                            "plaine-miner: share accepted ({} accepted, {} rejected)",
                            totals.accepted, totals.rejected
                        );
                    }
                }
            }
        }
    };

    shared.stop();
    drop(tx);
    for h in workers {
        let _ = h.join();
    }
    totals.hashes += shared.hashes();
    Ok(ended)
}

fn retarget(shared: &Arc<Shared>, target: [u8; 32]) {
    match shared.job() {
        Some(j) => shared.publish(JobView { target, ..(*j).clone() }),
        None => shared.publish(JobView { target, ..JobView::empty() }),
    }
}

/// A job id as the server spelled it, next to the u32 the miner sorts by.
///
/// Both halves are needed. The number is what workers and `Solution` carry, but a
/// stratum job id is an opaque string and `mining.submit` has to echo it back byte for
/// byte. Reformatting it - "2341" parsed and re-emitted as "00002341" - hands the server
/// an id it never issued. The node pads its own ids to eight hex digits, so that bug is
/// invisible against it and fatal against a pool that does not: rplant.xyz sends "2341"
/// and answered every reformatted submit with error 21, 0 accepted out of 35.
#[derive(Debug, Clone)]
struct JobId {
    id: u32,
    hex: String,
}

#[derive(Default)]
struct JobTrack {
    live: Vec<JobId>,
    displaced: Option<(JobId, Instant)>,
    newest: Option<u32>,
}

impl JobTrack {
    fn notify(&mut self, job_id: u32, hex: &str, clean: bool) {
        if clean {
            self.displaced = self.live.last().cloned().map(|j| (j, Instant::now()));
            self.live.clear();
        }
        self.live.push(JobId { id: job_id, hex: hex.to_string() });

        while self.live.len() > work::JOB_SLOTS {
            self.live.remove(0);
        }
        self.newest = Some(job_id);
    }

    fn submittable(&self, job_id: u32) -> bool {
        if self.live.iter().any(|j| j.id == job_id) {
            return true;
        }
        matches!(&self.displaced, Some((j, at)) if j.id == job_id && at.elapsed() < work::STALE_CREDIT_GRACE)
    }

    /// The server's own spelling of this job id, for `mining.submit` to echo.
    fn hex_of(&self, job_id: u32) -> Option<&str> {
        if let Some(j) = self.live.iter().find(|j| j.id == job_id) {
            return Some(&j.hex);
        }
        match &self.displaced {
            Some((j, _)) if j.id == job_id => Some(&j.hex),
            _ => None,
        }
    }
}

struct Status {
    every: Option<Duration>,
    last: Instant,
    last_hashes: u64,
    start: Instant,
    last_lanes: Vec<u64>,
    verbose: bool,
}

impl Status {
    fn new(secs: u64, start: Instant, verbose: bool) -> Status {
        Status {
            every: (secs > 0).then(|| Duration::from_secs(secs)),
            last: Instant::now(),
            last_hashes: 0,
            start,
            last_lanes: Vec::new(),
            verbose,
        }
    }

    fn maybe_print(&mut self, shared: &Shared, r: &Report, jobs: &JobTrack) {
        let Some(every) = self.every else { return };
        let elapsed = self.last.elapsed();
        if elapsed < every {
            return;
        }
        let hashes = shared.hashes();
        let rate = (hashes - self.last_hashes) as f64 / elapsed.as_secs_f64().max(1e-9);
        self.last = Instant::now();
        self.last_hashes = hashes;
        let job = shared.job();

        let up = self.start.elapsed();
        let avg = (r.hashes + hashes) as f64 / up.as_secs_f64().max(1e-9);

        println!(
            "plaine-miner: {} | accepted {} rejected {} blocks {} | job {} height {} | avg {} | up {}",
            rate_str(rate),
            r.accepted,
            r.rejected,
            r.blocks,
            jobs.newest.map(|j| format!("{j:08x}")).unwrap_or_else(|| "-".into()),
            job.as_ref().map(|j| j.height).unwrap_or(0),
            rate_str(avg),
            uptime_str(up),
        );
        if self.verbose {
            self.print_lanes(shared, elapsed);
        }
    }

    fn print_lanes(&mut self, shared: &Shared, elapsed: Duration) {
        let n = shared.lane_count();
        if n == 0 {
            return;
        }
        if self.last_lanes.len() < n {
            self.last_lanes.resize(n, 0);
        }
        let secs = elapsed.as_secs_f64().max(1e-9);

        for chunk in (0..n).collect::<Vec<_>>().chunks(8) {
            let mut line = String::from("plaine-miner:  ");
            for &i in chunk {
                let now = shared.lane(i);

                let d = now.saturating_sub(self.last_lanes[i]);
                self.last_lanes[i] = now;
                line.push_str(&format!(" {i:>3}:{:>10}", rate_str(d as f64 / secs)));
            }
            println!("{line}");
        }
    }
}

fn uptime_str(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s >= 86_400 => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3_600),
        s if s >= 3_600 => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

pub fn rate_str(h_per_s: f64) -> String {
    match h_per_s {
        r if r >= 1e9 => format!("{:.2} GH/s", r / 1e9),
        r if r >= 1e6 => format!("{:.2} MH/s", r / 1e6),
        r if r >= 1e3 => format!("{:.2} kH/s", r / 1e3),
        r => format!("{r:.0} H/s"),
    }
}

fn send(w: &mut TcpStream, args: &Args, line: &str) -> std::io::Result<()> {
    if args.verbose {
        eprintln!(">> {line}");
    }
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn spawn_worker(
    index: usize,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Solution>,
    mut pads: Pads,
    pin: Option<(usize, u16)>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("plaine-miner-{index}"))
        .spawn(move || {
            // pin the worker so its scratchpad stays local to one core; a thread that migrates
            // drags the pad through another core's cache and the hash rate drops.
            if let Some((cpu, group)) = pin {
                let placed = args::cpu::pin_current_thread(cpu, group);
                if !placed.is_pinned() {
                    eprintln!(
                        "plaine-miner: worker {index} asked for cpu {cpu}: {}",
                        placed.describe()
                    );
                }
            }
            let mut miner = match Miner::new() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("plaine-miner: worker {index} cannot start: {e}");
                    return;
                }
            };

            let mut nonces = [0u64; BATCH];
            let mut headers = [[0u8; 132]; BATCH];
            let mut seeds = [0u64; BATCH];
            let mut digests = [0u64; BATCH];
            let mut counter: u64 = 0;
            let mut seen_generation = u64::MAX;
            let mut job: Option<Arc<JobView>> = None;

            let e1 = shared.e1();
            let x_bits = shared.x_bits();
            let cbits = x_bits - work::THREAD_BITS;
            let cmask = (1u64 << cbits) - 1;

            while !shared.stopped() {
                let g = shared.generation();
                if g != seen_generation {
                    seen_generation = g;
                    job = shared.job();
                }
                let Some(j) = job.as_ref().filter(|j| j.live) else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };

                for s in 0..BATCH {
                    // worker index in the top cbits, counter below: disjoint nonce lanes per worker.
                    let x = ((index as u64) << cbits) | (counter & cmask);
                    counter = counter.wrapping_add(1);
                    let nonce = work::compose_in(e1, x, x_bits);
                    debug_assert!(work::owns_in(e1, nonce, x_bits));
                    nonces[s] = nonce;
                    headers[s] = work::assemble(&j.prefix, nonce);
                    seeds[s] = pow::seed(&headers[s]);
                }
                if miner
                    .mine_hash_batch_on(&mut pads, &seeds, &mut digests)
                    .is_err()
                {
                    eprintln!("plaine-miner: worker {index} JIT failure, stopping");
                    return;
                }
                shared.add_hashes_from(index, BATCH as u64);

                for s in 0..BATCH {
                    let h = pow::pow_hash(&headers[s], digests[s]);
                    if h <= j.target {
                        let sol = Solution {
                            job_id: j.job_id,
                            nonce: nonces[s],
                            pow_hash: h,
                            height: j.height,
                            is_block: h <= j.network_target,
                        };
                        if tx.send(sol).is_err() {
                            return;
                        }
                    }
                }
            }
        })
        .expect("spawning a miner worker")
}

pub fn grind(
    header: &[u8; 132],
    target_be: &[u8; 32],
    threads: usize,
    start_nonce: u64,
) -> Option<(u64, [u8; 32], u64)> {
    let found = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
    let hashes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut handles = Vec::new();

    let (pads, _err) = Pads::many(threads.max(1), BATCH, true);

    let spawned = pads.len().max(1) as u64;
    for (t, mut pads) in pads.into_iter().enumerate() {
        let found = found.clone();
        let hashes = hashes.clone();
        let header = *header;
        let target = *target_be;
        let threads = spawned;
        handles.push(std::thread::spawn(move || -> Option<(u64, [u8; 32])> {
            let mut miner = Miner::new().ok()?;
            let mut headers = [[0u8; 132]; BATCH];
            let mut seeds = [0u64; BATCH];
            let mut digests = [0u64; BATCH];

            let mut base = start_nonce.wrapping_add(t as u64);
            let mut local = 0u64;
            loop {
                if found.load(std::sync::atomic::Ordering::Relaxed) != u64::MAX {
                    hashes.fetch_add(local, std::sync::atomic::Ordering::Relaxed);
                    return None;
                }
                for s in 0..BATCH {
                    let n = base.wrapping_add((s as u64).wrapping_mul(threads));
                    headers[s] = header;
                    headers[s][124..].copy_from_slice(&n.to_le_bytes());
                    seeds[s] = pow::seed(&headers[s]);
                }
                miner
                    .mine_hash_batch_on(&mut pads, &seeds, &mut digests)
                    .ok()?;
                local += BATCH as u64;

                for s in 0..BATCH {
                    let ph = pow::pow_hash(&headers[s], digests[s]);
                    if ph <= target {
                        let n = base.wrapping_add((s as u64).wrapping_mul(threads));
                        found.store(n, std::sync::atomic::Ordering::SeqCst);
                        hashes.fetch_add(local, std::sync::atomic::Ordering::Relaxed);
                        return Some((n, ph));
                    }
                }
                base = base.wrapping_add((BATCH as u64).wrapping_mul(threads));
            }
        }));
    }
    let mut out = None;
    for h in handles {
        if let Ok(Some(v)) = h.join() {
            out = Some(v);
        }
    }
    out.map(|(n, ph)| (n, ph, hashes.load(std::sync::atomic::Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use plaine_pow::Scratch;

    #[test]
    fn grind_returns_a_valid_nonce() {
        let header = [0u8; 132];
        let mut target = [0xffu8; 32];
        target[0] = 0x00;

        let (nonce, ph, hashes) = grind(&header, &target, 4, 0).expect("a solution exists");
        assert!(ph <= target, "grind returned nonce {nonce} whose pow_hash exceeds the target");
        assert!(hashes >= 1);

        let mut h = header;
        h[124..].copy_from_slice(&nonce.to_le_bytes());
        let seed = pow::seed(&h);
        let mut miner = Miner::new().expect("hardware AES");
        let mut pad = Scratch::new();
        let d = miner.mine_hash(&mut pad, seed).expect("hash");
        assert_eq!(ph, pow::pow_hash(&h, d), "reported pow_hash does not match a fresh hash of that nonce");
    }

    #[test]
    fn grind_is_deterministic_and_lowest() {
        let header = [0u8; 132];
        let mut target = [0xffu8; 32];
        target[0] = 0x00;

        let a = grind(&header, &target, 1, 0).expect("solution").0;
        let b = grind(&header, &target, 1, 0).expect("solution").0;
        assert_eq!(a, b, "single-threaded grind is not reproducible - release.sh's probe gate needs it to be");

        let mut miner = Miner::new().expect("hardware AES");
        let mut pad = Scratch::new();
        for n in 0..a {
            let mut h = header;
            h[124..].copy_from_slice(&n.to_le_bytes());
            let d = miner.mine_hash(&mut pad, pow::seed(&h)).expect("hash");
            assert!(
                pow::pow_hash(&h, d) > target,
                "nonce {n} also meets the target but grind returned the higher {a}"
            );
        }
    }

    #[test]
    fn an_unpadded_job_id_comes_back_exactly_as_sent() {
        // rplant.xyz issues "2341", not "00002341". Parsing it to a u32 and re-emitting
        // it padded hands the server an id it never issued; it answered every one of 35
        // such submits with error 21 and accepted none.
        let mut j = JobTrack::default();
        for hex in ["2341", "00002341", "a", "FFFFFFFF", "0"] {
            let id = u32::from_str_radix(hex, 16).expect("test ids are hex");
            j.notify(id, hex, true);
            assert!(j.submittable(id));
            assert_eq!(
                j.hex_of(id),
                Some(hex),
                "the submit must echo the server's own spelling of {hex:?}"
            );
        }
    }

    #[test]
    fn a_displaced_job_keeps_its_spelling_through_the_grace() {
        let mut j = JobTrack::default();
        j.notify(0x2341, "2341", true);
        j.notify(0x2342, "2342", true);
        assert_eq!(j.hex_of(0x2341), Some("2341"), "a job inside its grace is still submittable");
        assert_eq!(j.hex_of(0x2342), Some("2342"));
        assert_eq!(j.hex_of(0x9999), None);
    }

    #[test]
fn clean_keeps_displaced_job_for_grace() {

        let mut j = JobTrack::default();
        j.notify(1, &format!("{:08x}", 1), true);
        j.notify(2, &format!("{:08x}", 2), true);
        assert!(j.submittable(2), "the current job");
        assert!(j.submittable(1), "displaced by a clean, still credited");
        assert!(!j.submittable(99), "never served");
    }

    #[test]
    fn jobs_since_clean_stay_submittable() {
        let mut j = JobTrack::default();
        j.notify(10, &format!("{:08x}", 10), true);
        for id in 11..=13 {
            j.notify(id, &format!("{:08x}", id), false);
        }
        for id in 10..=13 {
            assert!(j.submittable(id), "job {id} is still live on the server");
        }

        j.notify(14, &format!("{:08x}", 14), false);
        assert!(!j.submittable(10), "rotated out of a {}-slot ring", work::JOB_SLOTS);
        assert!(j.submittable(14));
    }

    #[test]
    fn clean_retires_older_jobs() {
        let mut j = JobTrack::default();
        j.notify(1, &format!("{:08x}", 1), true);
        j.notify(2, &format!("{:08x}", 2), false);
        j.notify(3, &format!("{:08x}", 3), false);
        j.notify(4, &format!("{:08x}", 4), true);
        assert!(j.submittable(4));
        assert!(j.submittable(3), "the displaced head keeps its grace");
        assert!(!j.submittable(2), "cleared by the clean");
        assert!(!j.submittable(1), "cleared by the clean");
    }

    #[test]
    fn hashrate_prints_with_a_unit() {
        assert_eq!(rate_str(0.0), "0 H/s");
        assert_eq!(rate_str(940.0), "940 H/s");
        assert_eq!(rate_str(1_400.0), "1.40 kH/s");
        assert_eq!(rate_str(2_500_000.0), "2.50 MH/s");
        assert_eq!(rate_str(3e9), "3.00 GH/s");
    }

    const BANSCORE_ERROR_24: f64 = 25.0;

    const BANSCORE_BAN_AT: f64 = 100.0;

    const BANSCORE_DECAY_SECS: f64 = 6.0;

    const CONNECTIONS_PER_MINUTE: usize = 6;

    fn peak_banscore(mut rungs: Rungs, attempts: usize) -> f64 {
        let mut score = 0.0f64;
        let mut peak = 0.0f64;
        for _ in 0..attempts {
            score += BANSCORE_ERROR_24;
            peak = peak.max(score);
            let waited = rungs.step().as_secs_f64();
            score = (score - waited / BANSCORE_DECAY_SECS).max(0.0);
        }
        peak
    }

    fn attempts_in_the_first_minute(mut rungs: Rungs) -> usize {
        let mut at = Duration::ZERO;
        let mut n = 0;
        while at < Duration::from_secs(60) {
            n += 1;
            at += rungs.step();
        }
        n
    }

    #[test]
    fn connection_ladder_doubles_to_cap() {
        let mut r = Rungs::new(BACKOFF_MIN, BACKOFF_MAX);
        let seen: Vec<Duration> = (0..9).map(|_| r.step()).collect();
        assert_eq!(seen[0], Duration::from_secs(1));
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[4], Duration::from_secs(16));
        assert_eq!(seen[5], BACKOFF_MAX, "16 doubles to 32, capped at 30");
        assert!(seen.iter().all(|d| *d <= BACKOFF_MAX));

        r.reset();
        assert_eq!(r.step(), BACKOFF_MIN);
    }

    #[test]
    fn auth_ladder_slower_than_connection() {
        let mut conn = Rungs::new(BACKOFF_MIN, BACKOFF_MAX);
        let mut auth = Rungs::new(AUTH_BACKOFF_MIN, AUTH_BACKOFF_MAX);
        for rung in 0..12 {
            let (c, a) = (conn.step(), auth.step());
            assert!(
                a > c,
                "rung {rung}: a refused login waits {a:?} and a dropped connection {c:?}; \
                 the two ladders have converged and the slow one has stopped being slow"
            );
        }

        assert!(AUTH_BACKOFF_MAX > BACKOFF_MAX);
        assert!(AUTH_BACKOFF_MIN > BACKOFF_MAX, "the slow ladder's first rung outlasts the fast one's last");
    }

    #[test]
    fn auth_ladder_stays_below_ban() {
        let peak = peak_banscore(Rungs::new(AUTH_BACKOFF_MIN, AUTH_BACKOFF_MAX), 64);
        assert!(
            peak < BANSCORE_BAN_AT,
            "a miner refused 64 times peaks at {peak} banscore against a threshold of \
             {BANSCORE_BAN_AT}; the ban rule would ban this IP for 600 s and then \
             for 2 400"
        );

        assert_eq!(peak, BANSCORE_ERROR_24, "the score should never carry from one attempt to the next");
        assert_eq!(
            AUTH_BACKOFF_MIN.as_secs_f64(),
            BANSCORE_ERROR_24 * BANSCORE_DECAY_SECS,
            "AUTH_BACKOFF_MIN is exactly the time error 24's +25 takes to decay away"
        );

        let fast = peak_banscore(Rungs::new(BACKOFF_MIN, BACKOFF_MAX), 5);
        assert!(
            fast >= BANSCORE_BAN_AT,
            "retrying a refusal on the connection ladder peaks at only {fast}; if that is \
             genuinely safe then the second ladder is not buying anything"
        );
    }

    #[test]
    fn ladders_respect_rate_limit() {
        let conn = attempts_in_the_first_minute(Rungs::new(BACKOFF_MIN, BACKOFF_MAX));
        assert!(
            conn <= CONNECTIONS_PER_MINUTE,
            "a miner reconnecting to a flapping server opens {conn} connections in its first \
             minute; the rate limit accepts {CONNECTIONS_PER_MINUTE} from one IP"
        );
        assert_eq!(
            attempts_in_the_first_minute(Rungs::new(AUTH_BACKOFF_MIN, AUTH_BACKOFF_MAX)),
            1,
            "a refused login should cost exactly one connection a minute, and not even that"
        );
    }

    #[test]
    fn ladders_are_independent() {
        let mut conn = Rungs::new(BACKOFF_MIN, BACKOFF_MAX);
        let mut auth = Rungs::new(AUTH_BACKOFF_MIN, AUTH_BACKOFF_MAX);
        for _ in 0..6 {
            auth.step();
        }
        assert_eq!(conn.step(), BACKOFF_MIN, "authorization failures climbed the connection ladder");

        let mut conn = Rungs::new(BACKOFF_MIN, BACKOFF_MAX);
        let mut auth = Rungs::new(AUTH_BACKOFF_MIN, AUTH_BACKOFF_MAX);
        for _ in 0..6 {
            conn.step();
        }
        assert_eq!(auth.step(), AUTH_BACKOFF_MIN, "outages climbed the authorization ladder");
    }

    #[test]
    fn no_workers_ends_before_first_hash() {
        let mut r = Report::default();
        assert!(
            no_workers_is_fatal(&r),
            "a miner that has never hashed and cannot map a scratchpad must stop, not loop \
             forever printing one line at an unattended rig"
        );
        r.hashes = 1;
        assert!(
            !no_workers_is_fatal(&r),
            "a miner that has mined has proved this machine can map pads; the failure is a \
             condition of the moment and it must wait it out"
        );

        let r = Report { connections: 1, ..Report::default() };
        assert!(no_workers_is_fatal(&r), "authorizing is not evidence that anything was mined");
    }

    #[test]
    fn condition_logged_once_then_on_change() {
        let mut said = None;
        assert!(say_once(&mut said, "server closed the connection"), "the first time always speaks");
        for _ in 0..300 {
            assert!(
                !say_once(&mut said, "server closed the connection"),
                "the same cause was reprinted; 300 identical lines bury the one that matters"
            );
        }
        assert!(say_once(&mut said, "authorize refused: 24 bad address"), "a changed cause must be reported");

        said = None;
        assert!(say_once(&mut said, "authorize refused: 24 bad address"), "the trouble came back and nothing was said");
    }

    #[test]
    fn all_workers_stopped_is_noticed() {
        assert!(
            !all_workers_stopped(&[]),
            "no workers yet is not all workers stopped; every session is in that state \
             until mining.authorize, and a vacuous truth here ends it before it begins"
        );

        let shared = Arc::new(Shared::new(1));
        shared.set_e1(0x00A3F2);
        let (tx, rx) = mpsc::channel::<Solution>();
        shared.publish(JobView {
            job_id: 1,
            job_id_hex: "00000001".into(),
            prefix: [0x5A; work::PREFIX_BYTES],
            target: [0xff; 32],
            network_target: [0u8; 32],
            height: 1,
            live: true,
        });
        let pads = Pads::new(BATCH, true).expect("map this worker's pads");
        let workers = vec![spawn_worker(0, Arc::clone(&shared), tx, pads, None)];
        assert!(!all_workers_stopped(&workers), "a worker that is mining has not stopped");

        drop(rx);
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !all_workers_stopped(&workers) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            all_workers_stopped(&workers),
            "every worker has returned and the session would go on holding the socket"
        );
        shared.stop();
        for h in workers {
            let _ = h.join();
        }
    }

    #[test]
    fn preflight_passes_when_mineable() {
        preflight().expect("this machine runs the miner's own test suite, so it has AES");
    }

    #[test]
    fn two_workers_mine_disjoint_nonces() {
        const UPSTREAM: u32 = 0x00A3F2;
        let x_bits = work::X_BITS_MIN;
        let e1 = (UPSTREAM << (32 - work::E1_BITS)) | 0x05;

        let shared = Arc::new(Shared::new(2));
        shared.set_window(e1, x_bits);
        let (tx, rx) = mpsc::channel::<Solution>();
        shared.publish(JobView {
            job_id: 1,
            job_id_hex: "00000001".into(),
            prefix: [0x5A; work::PREFIX_BYTES],
            target: [0xff; 32],
            network_target: [0u8; 32],
            height: 7,
            live: true,
        });

        let mut handles = Vec::new();
        for i in 0..2 {
            let pads = Pads::new(BATCH, true).expect("map this worker's pads");
            handles.push(spawn_worker(i, Arc::clone(&shared), tx.clone(), pads, None));
        }
        drop(tx);

        let mut got = Vec::new();
        while got.len() < 6 * BATCH {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(s) => got.push(s.nonce),
                Err(_) => panic!("a worker produced no solution in 30 s"),
            }
        }
        shared.stop();
        for h in handles {
            let _ = h.join();
        }
        assert!(shared.lane(0) > 0 && shared.lane(1) > 0, "only one of the two workers ran");

        let mut seen = std::collections::HashSet::new();
        for n in &got {
            assert!(
                work::owns_in(e1, *n, x_bits),
                "{n:#018x} is outside this rig's sub-slice: the worker rolled bits the proxy \
                 kept, and a sibling rig is mining them too"
            );

            assert!(work::owns(UPSTREAM, *n), "{n:#018x} left the upstream 24-bit slice");
            assert!(seen.insert(*n), "{n:#018x} was mined twice");
        }

        let lane = |n: u64| (n >> (x_bits - work::THREAD_BITS)) & 0xFF;
        assert!(got.iter().any(|n| lane(*n) == 0), "worker 0 never appeared in the stream");
        assert!(got.iter().any(|n| lane(*n) == 1), "worker 1 never appeared in the stream");
    }

    #[test]
    fn defaults_are_zero_config() {
        let a = Args::default();
        assert_eq!(a.stratum, "127.0.0.1:9258");
        assert!(a.reconnect, "a miner that stops at the first hiccup is not zero-config");
        assert_eq!(a.status_secs, 10);
        assert!(a.threads >= 1);
        assert!(a.login.is_empty(), "the payee has no honest default");
    }

    #[test]
    fn deadline_shortens_backoff() {
        let args = Args { deadline_secs: 2, ..Args::default() };
        let start = Instant::now();
        let left = remaining(&args, start).expect("a deadline was set");
        assert!(left <= Duration::from_secs(2));
        assert!(remaining(&Args::default(), start).is_none(), "no deadline, no limit");
    }

    #[test]
    fn limits_span_reconnects() {
        let args = Args { max_shares: 2, ..Args::default() };
        let start = Instant::now();
        let mut r = Report::default();
        assert!(!limit_reached(&args, start, &r));
        r.accepted = 2;
        assert!(limit_reached(&args, start, &r), "totals accumulate across sessions");

        let args = Args { max_blocks: 1, ..Args::default() };
        let mut r = Report::default();
        assert!(!limit_reached(&args, start, &r));
        r.blocks = 1;
        assert!(limit_reached(&args, start, &r));
    }

    #[test]
    fn renotified_job_does_not_restart() {
        let shared = Arc::new(Shared::new(1));
        shared.set_e1(0x00A3F2);
        let (tx, rx) = mpsc::channel::<Solution>();

        let job = |job_id: u32| JobView {
            job_id,
            job_id_hex: format!("{job_id:08x}"),
            prefix: [0x5A; work::PREFIX_BYTES],
            target: [0xff; 32],
            network_target: [0u8; 32],
            height: 7,
            live: true,
        };

        shared.publish(job(0));
        let pads = Pads::new(BATCH, true).expect("map this worker's pads");
        let h = spawn_worker(0, Arc::clone(&shared), tx, pads, None);

        let take = |rx: &mpsc::Receiver<Solution>, n: usize| -> Vec<u64> {
            let mut out = Vec::new();
            while out.len() < n {
                match rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(s) => out.push(s.nonce),
                    Err(_) => panic!("the worker produced no solution in 30 s"),
                }
            }
            out
        };

        let mut got = take(&rx, BATCH);
        shared.publish(job(1));
        got.extend(take(&rx, 5 * BATCH));
        shared.stop();
        let _ = h.join();

        assert!(
            got.iter().all(|n| work::owns(0x00A3F2, *n)),
            "every nonce must stay inside the connection's slice"
        );

        for pair in got.windows(2) {
            let (a, b) = (pair[0] as u32, pair[1] as u32);
            assert_eq!(
                b,
                a.wrapping_add(1),
                "the nonce counter did not advance contiguously across the job \
                 change: {a:#010x} -> {b:#010x}. A re-notify must not restart the \
                 search (nor the batch seam skip a nonce)."
            );
        }
    }
}
