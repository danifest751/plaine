use crate::abuse::{Admit, Severity};
use crate::limits::{ADMIT_IN_FLIGHT, ADMIT_QUEUED, READ_BUF_INITIAL};
use crate::metrics::Metrics;
use crate::session::{Action, CloseReason, Session, Shared};
use crate::verify::{ConnId, ResultSink, VerifyResult};
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::{timeout, Duration};

const RESULT_CHANNEL_DEPTH: usize = 2 * (ADMIT_IN_FLIGHT + ADMIT_QUEUED);

const READ_CHUNK: usize = 1024;

const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

pub struct Router {
    conns: Mutex<HashMap<ConnId, mpsc::Sender<VerifyResult>>>,
}

impl Router {
    pub fn new() -> Router {
        Router {
            conns: Mutex::new(HashMap::new()),
        }
    }

    pub fn sink(self: &Arc<Self>) -> ResultSink {
        let me = Arc::clone(self);
        Arc::new(move |r: VerifyResult| {
            let tx = match me.conns.lock() {
                Ok(m) => m.get(&r.conn).cloned(),
                Err(_) => None,
            };
            // try_send, never block. A verdict for a gone or wedged connection
            // is dropped instead of stalling a verification thread on it.
            if let Some(tx) = tx {
                let _ = tx.try_send(r);
            }
        })
    }

    fn register(&self, id: ConnId, tx: mpsc::Sender<VerifyResult>) {
        if let Ok(mut m) = self.conns.lock() {
            m.insert(id, tx);
        }
    }

    fn unregister(&self, id: ConnId) {
        if let Ok(mut m) = self.conns.lock() {
            m.remove(&id);
        }
    }

    pub fn len(&self) -> usize {
        self.conns.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Router {
    fn default() -> Self {
        Router::new()
    }
}

pub struct StratumServer {
    shared: Arc<Shared>,
    router: Arc<Router>,
    live: Arc<AtomicUsize>,
    next_id: AtomicU64,
    gen_tx: watch::Sender<u64>,
    gen_rx: watch::Receiver<u64>,
    revoke_tx: watch::Sender<u64>,
    revoke_rx: watch::Receiver<u64>,
    revoke_seq: AtomicU64,
    start: Instant,
    shutdown: Notify,
    stopping: AtomicUsize,
    // Live per-connection snapshots for the node's read-only introspection RPC.
    // Written on the session's own tick and cleared when the connection drops.
    cards: Mutex<HashMap<ConnId, crate::session::SessionCard>>,
}

impl StratumServer {
    pub fn new(shared: Arc<Shared>, router: Arc<Router>) -> Arc<StratumServer> {
        let (gen_tx, gen_rx) = watch::channel(0u64);
        let (revoke_tx, revoke_rx) = watch::channel(0u64);
        Arc::new(StratumServer {
            shared,
            router,
            live: Arc::new(AtomicUsize::new(0)),
            next_id: AtomicU64::new(1),
            gen_tx,
            gen_rx,
            revoke_tx,
            revoke_rx,
            revoke_seq: AtomicU64::new(0),
            start: Instant::now(),
            shutdown: Notify::new(),
            stopping: AtomicUsize::new(0),
            cards: Mutex::new(HashMap::new()),
        })
    }

    /// A snapshot of every live session. Read-only; safe to call from the RPC
    /// thread. Pair each card's monotonic-ms timestamps with `now_ms()`.
    pub fn session_cards(&self) -> Vec<crate::session::SessionCard> {
        self.cards.lock().expect("cards mutex").values().cloned().collect()
    }

    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn live_connections(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn notify_generation(&self) {
        let gen = self.shared.jobs.generation();

        self.gen_tx.send_replace(gen);
    }

    pub fn notify_revocation(&self) {
        let n = self.revoke_seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.revoke_tx.send_replace(n);
    }

    pub fn revocations_published(&self) -> u64 {
        self.revoke_seq.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stopping.store(1, Ordering::SeqCst);
        self.shutdown.notify_waiters();
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst) != 0
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        self.serve_on(listener).await
    }

    pub async fn serve_on<A: Accept>(self: Arc<Self>, listener: A) -> io::Result<()> {
        loop {
            let accepted = tokio::select! {
                biased;
                _ = self.shutdown.notified() => return Ok(()),
                r = listener.accept() => r,
            };
            if self.is_stopping() {
                return Ok(());
            }
            let (stream, peer) = match accepted {
                Ok(v) => v,
                Err(e) => {
                    if is_fatal_accept_error(&e) {
                        return Err(e);
                    }
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            };
            let ip = peer.ip();
            let now = self.now_ms();

            let verdict = {
                let mut bans = match self.shared.bans.lock() {
                    Ok(b) => b,
                    Err(_) => return Ok(()),
                };
                let mut accept = match self.shared.accept.lock() {
                    Ok(a) => a,
                    Err(_) => return Ok(()),
                };
                bans.admit(
                    ip,
                    now,
                    &self.shared.caps,
                    self.live.load(Ordering::Relaxed),
                    &mut accept,
                )
            };
            // a refused peer is dropped here, before we spawn a task or read a
            // byte, and each refusal is counted in its own class for the operator.
            if verdict != Admit::Ok {
                let m = &self.shared.metrics;
                match verdict {
                    Admit::Banned | Admit::Throttled => Metrics::inc(&m.refused_banned),
                    Admit::PerIpLimit => Metrics::inc(&m.refused_per_ip),
                    Admit::PerIpRate | Admit::AcceptRate => Metrics::inc(&m.refused_rate),
                    Admit::ServerFull => Metrics::inc(&m.refused_full),
                    Admit::Ok => {}
                }

                drop(stream);
                continue;
            }

            Metrics::inc(&self.shared.metrics.accepted);
            self.live.fetch_add(1, Ordering::Relaxed);
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let srv = Arc::clone(&self);
            tokio::spawn(async move {
                let _ = stream.set_nodelay(true);
                srv.connection(stream, ip, id).await;
            });
        }
    }

    async fn connection(self: Arc<Self>, stream: TcpStream, ip: IpAddr, id: ConnId) {
        let (results_tx, mut results_rx) = mpsc::channel(RESULT_CHANNEL_DEPTH);
        self.router.register(id, results_tx);

        let mut guard = ConnGuard {
            srv: Arc::clone(&self),
            id,
            session: Session::new(id, ip, self.now_ms(), &self.shared),
        };
        let session = &mut guard.session;
        self.cards.lock().expect("cards mutex").insert(id, session.card());

        let tick_ms = self.shared.cfg.tick_ms.max(1);
        let tick = Duration::from_millis(tick_ms);

        let write_timeout = if self.shared.cfg.write_timeout_ms == 0 {
            Duration::from_secs(31_536_000)
        } else {
            Duration::from_millis(self.shared.cfg.write_timeout_ms)
        };
        let out_buf_cap = self.shared.cfg.out_buf_cap;
        let (mut rd, mut wr) = stream.into_split();
        let mut inbuf: Vec<u8> = Vec::with_capacity(READ_BUF_INITIAL);
        let mut chunk = [0u8; READ_CHUNK];
        let mut gen_rx = self.gen_rx.clone();

        gen_rx.mark_unchanged();
        let mut revoke_rx = self.revoke_rx.clone();

        revoke_rx.mark_unchanged();
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_read_ms = self.now_ms();
        let mut last_tick_ms = self.now_ms();

        let close = loop {
            if self.is_stopping() {
                break Some(CloseReason::Shutdown);
            }
            // Unbiased select over reads, verdicts, the tick, and two watch
            // channels. Revocation gets its own channel so a stranded rig is hung
            // up in milliseconds, not whenever the next 2 s tick happens to land.
            let event = tokio::select! {
                _ = self.shutdown.notified() => Event::Shutdown,
                r = results_rx.recv() => match r {
                    Some(v) => Event::Result(v),
                    None => Event::Shutdown,
                },
                r = rd.read(&mut chunk) => match r {
                    Ok(0) => Event::Eof,
                    Ok(n) => Event::Bytes(n),
                    Err(_) => Event::Eof,
                },
                _ = ticker.tick() => Event::Tick,
                r = gen_rx.changed() => match r {
                    Ok(()) => Event::NewGeneration,
                    Err(_) => Event::Shutdown,
                },
                r = revoke_rx.changed() => match r {
                    Ok(()) => Event::Revocation,
                    Err(_) => Event::Shutdown,
                },
            };

            let now = self.now_ms();
            let mut closing = None;
            match event {
                Event::Shutdown => break Some(CloseReason::Shutdown),
                Event::Eof => break None,
                Event::Bytes(n) => {
                    last_read_ms = now;
                    inbuf.extend_from_slice(&chunk[..n]);

                    closing = feed_lines(session, &mut inbuf, now, &self.shared);
                }
                Event::Result(v) => session.on_verify_result(v, now, &self.shared),
                Event::Tick => {}
                Event::Revocation => {
                    let _ = session.check_slice(&self.shared);
                }
                Event::NewGeneration => {
                    if session.is_authorized() {
                        session.push_job(now, &self.shared, false);
                    }
                }
            }

            // run the tick on its own cadence even while bytes are arriving, so a
            // chatty peer cannot starve idle/deadline eviction.
            if now.saturating_sub(last_tick_ms) >= tick_ms {
                last_tick_ms = now;
                if self.shared.cfg.read_deadline_ms != 0
                    && now.saturating_sub(last_read_ms) > self.shared.cfg.read_deadline_ms
                {
                    break Some(CloseReason::ReadTimeout);
                }
                session.on_tick(now, &self.shared);
                self.cards.lock().expect("cards mutex").insert(id, session.card());
            }

            for action in session.take_actions() {
                match action {
                    Action::Verify(work) => {
                        let request_id = work.request_id;
                        if !self.shared.verifier.enqueue(*work) {
                            session.on_verify_refused(request_id, &self.shared);
                        }
                    }
                    Action::Close(r) => closing = closing.or(Some(r)),
                }
            }

            if let Err(r) = flush(&mut wr, &mut session.outbuf, write_timeout, out_buf_cap).await {
                break Some(r);
            }
            if let Some(r) = closing {
                break Some(r);
            }
        };

        let _ = close;
        let _ = wr.shutdown().await;
    }
}

struct ConnGuard {
    srv: Arc<StratumServer>,
    id: ConnId,
    session: Session,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.srv.router.unregister(self.id);
        self.srv.cards.lock().expect("cards mutex").remove(&self.id);
        let now = self.srv.now_ms();
        self.session.release(now, &self.srv.shared);
        self.srv.live.fetch_sub(1, Ordering::Relaxed);
    }
}

enum Event {
    Bytes(usize),
    Result(VerifyResult),
    Tick,
    NewGeneration,
    Revocation,
    Eof,
    Shutdown,
}

fn feed_lines(
    session: &mut Session,
    inbuf: &mut Vec<u8>,
    now_ms: u64,
    shared: &Arc<Shared>,
) -> Option<CloseReason> {
    loop {
        let limit = session.line_limit(&shared.cfg);
        match inbuf.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                if pos > limit {
                    return Some(oversize(session, shared, now_ms));
                }

                let end = if pos > 0 && inbuf[pos - 1] == b'\r' {
                    pos - 1
                } else {
                    pos
                };
                let line: Vec<u8> = inbuf[..end].to_vec();
                inbuf.drain(..=pos);
                if line.is_empty() {
                    if !session.charge_line(now_ms, shared) {
                        return Some(CloseReason::LineFlood);
                    }
                    continue;
                }
                session.on_line(&line, now_ms, shared);
                for action in &session.actions {
                    if let Action::Close(r) = action {
                        return Some(*r);
                    }
                }
            }
            None => {
                if inbuf.len() > limit {
                    return Some(oversize(session, shared, now_ms));
                }
                return None;
            }
        }
    }
}

fn oversize(session: &mut Session, shared: &Arc<Shared>, now_ms: u64) -> CloseReason {
    Metrics::inc(&shared.metrics.bad_json);
    if let Ok(mut b) = shared.bans.lock() {
        let _ = b.penalise(session.ip, 50, Severity::Hard, now_ms);
    }
    if session.is_authorized() {
        CloseReason::BadMessageAfterAuth
    } else {
        CloseReason::GarbageBeforeAuth
    }
}

async fn flush(
    wr: &mut OwnedWriteHalf,
    out: &mut Vec<u8>,
    write_timeout: Duration,
    out_buf_cap: usize,
) -> Result<(), CloseReason> {
    if out.is_empty() {
        return Ok(());
    }
    // a client that never drains its socket must be closed, not buffered without
    // bound; drop the over-cap bytes so carrying them forward cannot grow.
    if out_buf_cap != 0 && out.len() > out_buf_cap {
        out.clear();
        return Err(CloseReason::SlowClient);
    }
    match timeout(write_timeout, wr.write_all(out)).await {
        Ok(Ok(())) => {
            out.clear();
            Ok(())
        }

        Ok(Err(_)) | Err(_) => Err(CloseReason::SlowClient),
    }
}

pub trait Accept {
    fn accept(
        &self,
    ) -> impl std::future::Future<Output = io::Result<(TcpStream, std::net::SocketAddr)>> + Send;
}

impl Accept for TcpListener {
    fn accept(
        &self,
    ) -> impl std::future::Future<Output = io::Result<(TcpStream, std::net::SocketAddr)>> + Send
    {
        TcpListener::accept(self)
    }
}

// transient accept errors (one bad peer) are backed off and retried; only an
// unusable listener is fatal, so we do not spin retrying a syscall that cannot
// succeed and do not die on a single peer.
fn is_fatal_accept_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
    )
}

#[cfg(all(test, feature = "mock"))]
mod tests {
    use super::*;
    use crate::job::JobSource;
    use crate::limits::{Caps, Mode, MAX_LINE_PRE_AUTH, OUT_BUF_CAP, WRITE_TIMEOUT};
    use crate::mock::{MockJobSource, MockPow};
    use crate::nonce::{E1Allocator, E1};
    use crate::session::ServerConfig;
    use crate::target::Target;
    use crate::verify::ThreadPoolVerifier;
    use plaine_consensus::bech32m;
    use plaine_consensus::constants::ADDRESS_HRP;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::TcpSocket;

    struct Rig {
        srv: Arc<StratumServer>,
        src: Arc<MockJobSource>,
        addr: std::net::SocketAddr,
    }

    async fn rig() -> Rig {
        rig_with(ServerConfig::for_mode(Mode::Pool)).await
    }

    async fn quick_rig() -> Rig {
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.tick_ms = 20;
        cfg.auth_deadline_ms = 300;
        cfg.idle_evict_ms = 600;
        rig_with(cfg).await
    }

    async fn rig_with(cfg: ServerConfig) -> Rig {
        rig_full(cfg, Arc::new(Mutex::new(E1Allocator::new()))).await
    }

    async fn rig_full(cfg: ServerConfig, e1: Arc<dyn crate::nonce::SliceSource>) -> Rig {
        rig_full_caps(cfg, Caps::for_mode(Mode::Pool), e1).await
    }

    struct ScriptedAcceptor {
        kind: io::ErrorKind,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        fatal_after: usize,
    }

    impl Accept for ScriptedAcceptor {
        fn accept(
            &self,
        ) -> impl std::future::Future<Output = io::Result<(TcpStream, std::net::SocketAddr)>> + Send
        {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let k = if n >= self.fatal_after { io::ErrorKind::InvalidInput } else { self.kind };
            async move { Err(io::Error::from(k)) }
        }
    }

    #[tokio::test]
    async fn transient_accept_error_backs_off() {
        let (srv, _l, _a) = unspawned(ServerConfig::for_mode(Mode::Pool)).await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let acc = ScriptedAcceptor {
            kind: io::ErrorKind::ConnectionAborted,
            calls: Arc::clone(&calls),
            fatal_after: 4,
        };
        assert!(!is_fatal_accept_error(&io::Error::from(io::ErrorKind::ConnectionAborted)));

        let t0 = Instant::now();
        let r = timeout(Duration::from_secs(10), Arc::clone(&srv).serve_on(acc))
            .await
            .expect("serve_on never returned even after a fatal accept error");
        let took = t0.elapsed();
        assert!(r.is_err(), "the scripted fatal error did not end the loop");

        let n = calls.load(Ordering::SeqCst);
        assert_eq!(n, 4, "the loop did not make the four accept calls the script hands it");
        assert!(
            took >= Duration::from_millis(250),
            "three transient errors took {took:?}; the backoff is not being applied ({:?}/retry)",
            ACCEPT_BACKOFF
        );
        assert!(
            took < Duration::from_secs(5),
            "three retries took {took:?}; backoff has grown unbounded"
        );
        println!("  S2: 4 accept() calls, 3 backoffs, {took:?} (ACCEPT_BACKOFF {ACCEPT_BACKOFF:?})");
    }

    #[tokio::test]
    async fn fatal_accept_error_ends_loop() {
        let (srv, _l, _a) = unspawned(ServerConfig::for_mode(Mode::Pool)).await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let acc = ScriptedAcceptor {
            kind: io::ErrorKind::InvalidInput,
            calls: Arc::clone(&calls),
            fatal_after: usize::MAX,
        };
        assert!(is_fatal_accept_error(&io::Error::from(io::ErrorKind::InvalidInput)));

        let r = timeout(Duration::from_secs(3), Arc::clone(&srv).serve_on(acc))
            .await
            .expect("serve_on never returned on a fatal accept error");
        let e = r.expect_err("serve_on returned Ok on a fatal accept error");
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "the error was not the one accept gave");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a fatal accept error was retried before the loop gave up"
        );
        println!("  S3: fatal accept error returned Err({:?}) after 1 call", e.kind());
    }

    async fn unspawned(cfg: ServerConfig) -> (Arc<StratumServer>, TcpListener, std::net::SocketAddr) {
        let src = Arc::new(MockJobSource::new(184_602, Target::from_difficulty(1 << 40)));
        let router = Arc::new(Router::new());
        let verifier = Arc::new(ThreadPoolVerifier::new(
            2,
            1024,
            Arc::new(MockPow::new()),
            router.sink(),
        ));
        let shared = Arc::new(Shared {
            bans: Mutex::new(crate::abuse::BanTable::new()),
            e1: Arc::new(Mutex::new(E1Allocator::new())),
            diffs: Mutex::new(crate::abuse::DiffCache::new()),
            accept: Mutex::new(crate::abuse::TokenBucket::new(500.0, 500.0, 0)),
            jobs: Arc::clone(&src) as Arc<dyn JobSource>,
            verifier,
            metrics: Metrics::default(),
            caps: Caps::for_mode(Mode::Pool),
            cfg,
        });
        let srv = StratumServer::new(shared, router);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        (srv, listener, addr)
    }

    async fn rig_full_caps(
        cfg: ServerConfig,
        caps: Caps,
        e1: Arc<dyn crate::nonce::SliceSource>,
    ) -> Rig {
        let src = Arc::new(MockJobSource::new(184_602, Target::from_difficulty(1 << 40)));
        let router = Arc::new(Router::new());
        let verifier = Arc::new(ThreadPoolVerifier::new(
            2,
            1024,
            Arc::new(MockPow::new()),
            router.sink(),
        ));
        let shared = Arc::new(Shared {
            bans: Mutex::new(crate::abuse::BanTable::new()),
            e1,
            diffs: Mutex::new(crate::abuse::DiffCache::new()),
            accept: Mutex::new(crate::abuse::TokenBucket::new(500.0, 500.0, 0)),
            jobs: Arc::clone(&src) as Arc<dyn JobSource>,
            verifier,
            metrics: Metrics::default(),
            caps,
            cfg,
        });
        let srv = StratumServer::new(shared, router);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let s2 = Arc::clone(&srv);
        tokio::spawn(async move {
            let _ = s2.serve(listener).await;
        });
        Rig { srv, src, addr }
    }

    async fn expect_closed<R: AsyncReadExt + Unpin>(rd: &mut R, what: &str) {
        let mut buf = [0u8; 64];
        match timeout(Duration::from_secs(5), rd.read(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("{what}"))
        {
            Ok(0) | Err(_) => {}
            Ok(n) => panic!("{what}: still connected, and it sent {n} bytes"),
        }
    }

    fn login() -> String {
        format!(
            "{}.rig1",
            bech32m::encode_bytes(ADDRESS_HRP, &[7u8; 20]).expect("addr")
        )
    }

    #[tokio::test]
    async fn miner_subscribes_and_gets_work() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();

        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t/1.0\"]}\n")
            .await
            .expect("write");
        let sub = lines.next_line().await.expect("io").expect("line");
        assert!(sub.contains("mining.notify"), "{sub}");
        assert!(sub.contains("mining.set_target"), "{sub}");

        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");

        let ok = lines.next_line().await.expect("io").expect("line");
        assert!(ok.contains("\"result\":true"), "{ok}");
        let target = lines.next_line().await.expect("io").expect("line");
        assert!(target.contains("mining.set_target"), "{target}");
        let notify = lines.next_line().await.expect("io").expect("line");
        assert!(notify.contains("mining.notify"), "{notify}");

        assert!(notify.contains(&"0".repeat(248)), "{notify}");
        r.srv.stop();
    }

    #[tokio::test]
    async fn new_tip_reaches_conn_before_tick() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let _ = lines.next_line().await;
        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
        for _ in 0..3 {
            let _ = lines.next_line().await;
        }

        r.src.new_tip(184_603);
        r.srv.notify_generation();

        let got = timeout(Duration::from_millis(500), lines.next_line())
            .await
            .expect("notify inside the 100 ms budget, not the 2 s tick")
            .expect("io")
            .expect("line");
        assert!(got.contains("mining.notify"), "{got}");
        assert!(got.contains("true"), "a new tip must clean jobs: {got}");
        r.srv.stop();
    }

    #[tokio::test]
    async fn garbage_before_authorize_disconnects() {
        let r = rig().await;
        let mut stream = TcpStream::connect(r.addr).await.expect("connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        expect_closed(&mut stream, "the server must not hold an HTTP prober open").await;
        r.srv.stop();
    }

    #[tokio::test]
    async fn oversized_line_not_buffered() {
        let r = rig().await;
        let mut stream = TcpStream::connect(r.addr).await.expect("connect");

        let flood = vec![b'a'; 64 * 1024];
        let _ = stream.write_all(&flood).await;
        expect_closed(&mut stream, "an unterminated line must not be accumulated").await;
        r.srv.stop();
    }

    #[tokio::test]
    async fn newline_stream_no_auth_hold() {
        let r = quick_rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (mut rd, mut wr) = stream.into_split();
        let pump = tokio::spawn(async move {
            loop {
                if wr.write_all(b"\n").await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        expect_closed(
            &mut rd,
            "a peer streaming newlines outlived its authorization deadline",
        )
        .await;
        pump.abort();
        r.srv.stop();
    }

    #[tokio::test]
    async fn tick_runs_while_bytes_arrive() {
        let r = quick_rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let _ = lines.next_line().await;
        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
        for _ in 0..3 {
            let _ = lines.next_line().await;
        }
        let chatter = tokio::spawn(async move {
            loop {
                if wr
                    .write_all(b"{\"id\":3,\"method\":\"mining.configure\",\"params\":[]}\n")
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match timeout(Duration::from_millis(200), lines.next_line()).await {
                Ok(Ok(None)) | Ok(Err(_)) => break,
                Ok(Ok(Some(_))) => {}
                Err(_) => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a chatty peer with no accepted shares was never evicted, \
                 so on_tick is not running while bytes arrive"
            );
        }
        chatter.abort();
        r.srv.stop();
    }

    #[tokio::test]
    async fn refusal_written_before_close() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let _ = lines.next_line().await;
        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
        for _ in 0..3 {
            let _ = lines.next_line().await;
        }

        wr.write_all(b"\x16\x03\x01\x00\xa5 not json\n")
            .await
            .expect("write");
        let got = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("the refusal must reach the wire before the FIN")
            .expect("io")
            .expect("line");
        assert!(got.contains("\"error\":[30"), "{got}");
        r.srv.stop();
    }

    #[tokio::test]
    async fn close_returns_slice_and_slot() {
        let r = quick_rig().await;
        for _ in 0..5 {
            let stream = TcpStream::connect(r.addr).await.expect("connect");
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
                .await
                .expect("write");
            let _ = timeout(Duration::from_secs(5), lines.next_line()).await;
            drop(wr);
            drop(lines);
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let live = r.srv.live_connections();
            let slices = r.srv.shared().e1.live();
            if live == 0 && slices == 0 && r.srv.router.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "leaked: {live} connections, {slices} nonce slices, {} router entries",
                r.srv.router.len()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        r.srv.stop();
    }

    struct Upstream {
        held: Mutex<Option<crate::nonce::E1>>,
        out: Mutex<Vec<(u32, u32)>>,
        next: AtomicUsize,
    }

    impl Upstream {
        fn new(e1: u32) -> Arc<Upstream> {
            Arc::new(Upstream {
                held: Mutex::new(Some(crate::nonce::E1(e1))),
                out: Mutex::new(Vec::new()),
                next: AtomicUsize::new(0),
            })
        }

        fn rebind(&self, e1: u32) {
            *self.held.lock().unwrap() = Some(crate::nonce::E1(e1));
            self.next.store(0, Ordering::SeqCst);
        }
    }

    impl crate::nonce::SliceSource for Upstream {
        fn acquire(&self) -> Option<(crate::nonce::E1, u32)> {
            let e1 = (*self.held.lock().unwrap())?;
            let sub = self.next.fetch_add(1, Ordering::SeqCst) as u32;
            self.out.lock().unwrap().push((e1.0, sub));
            Some((e1, sub))
        }
        fn release(&self, e1: crate::nonce::E1, sub: u32, _searched: bool) {
            self.out.lock().unwrap().retain(|p| *p != (e1.0, sub));
        }
        fn sub_bits(&self) -> u32 {
            8
        }
        fn live(&self) -> usize {
            self.out.lock().unwrap().len()
        }
        fn holds(&self, e1: crate::nonce::E1, sub: u32) -> bool {
            *self.held.lock().unwrap() == Some(e1)
                && self.out.lock().unwrap().contains(&(e1.0, sub))
        }
    }

    #[tokio::test]
    async fn revocation_hangs_up_promptly() {
        let up = Upstream::new(0x0001b5);
        let r = rig_full(
            ServerConfig::for_mode(Mode::Pool),
            Arc::clone(&up) as Arc<dyn crate::nonce::SliceSource>,
        )
        .await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let sub = lines.next_line().await.expect("io").expect("line");

        assert!(sub.contains("\"0001b500\",4"), "{sub}");
        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
        for _ in 0..3 {
            let _ = lines.next_line().await;
        }
        assert_eq!(r.srv.shared().e1.live(), 1);

        let t0 = std::time::Instant::now();
        up.rebind(0x000000);
        r.srv.notify_revocation();

        let got = timeout(Duration::from_millis(500), lines.next_line())
            .await
            .expect("a stranded rig must be hung up on, not left mining")
            .expect("io")
            .expect("line");
        assert!(got.contains("\"error\":[31,\"slice revoked\""), "{got}");
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "the hangup waited {elapsed:?}; the tick is 2 s and one block is 3.6 s"
        );
        expect_closed(lines.get_mut(), "the socket must follow the error").await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while r.srv.shared().e1.live() != 0 {
            assert!(std::time::Instant::now() < deadline, "the grant leaked");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(r.srv.revocations_published(), 1);
        assert_eq!(Metrics::get(&r.srv.shared().metrics.closed_slice_revoked), 1);
        r.srv.stop();
    }

    #[tokio::test]
    async fn tick_finds_revocation_unaided() {
        let up = Upstream::new(0x0001b5);
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.tick_ms = 50;
        let r = rig_full(cfg, Arc::clone(&up) as Arc<dyn crate::nonce::SliceSource>).await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let _ = lines.next_line().await;
        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
        for _ in 0..3 {
            let _ = lines.next_line().await;
        }

        up.rebind(0x000000);

        let got = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("the tick must find it unaided")
            .expect("io")
            .expect("line");
        assert!(got.contains("\"error\":[31,\"slice revoked\""), "{got}");
        r.srv.stop();
    }

    #[tokio::test]
    async fn keepalives_beat_read_deadline() {
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.tick_ms = 20;
        cfg.read_deadline_ms = 400;
        cfg.idle_evict_ms = 60_000;
        let r = rig_with(cfg).await;

        let quiet = TcpStream::connect(r.addr).await.expect("connect");
        let (qrd, mut qwr) = quiet.into_split();
        let mut qlines = BufReader::new(qrd).lines();
        qwr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let _ = qlines.next_line().await;

        let alive = TcpStream::connect(r.addr).await.expect("connect");
        let (ard, mut awr) = alive.into_split();
        let mut alines = BufReader::new(ard).lines();
        awr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let _ = alines.next_line().await;

        let t0 = std::time::Instant::now();
        let mut acks = 0u32;

        while t0.elapsed() < Duration::from_millis(1_200) {
            awr.write_all(b"{\"id\":9,\"method\":\"mining.keepalive\"}\n")
                .await
                .expect("the keepalived socket must still be writable");
            let ack = timeout(Duration::from_millis(500), alines.next_line())
                .await
                .expect("an answer, so a half-open socket is detectable")
                .expect("io")
                .expect("line");
            assert_eq!(ack, "{\"id\":9,\"result\":true,\"error\":null}");
            acks += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(acks >= 10, "only {acks} heartbeats round-tripped");
        assert_eq!(Metrics::get(&r.srv.shared().metrics.keepalives), acks as u64);

        expect_closed(
            qlines.get_mut(),
            "a socket that said nothing outlived the read deadline",
        )
        .await;
        r.srv.stop();
    }

    #[tokio::test]
    async fn dead_reader_closed_not_queued() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");

        for h in 0..200u64 {
            r.src.new_tip(184_603 + h);
            r.srv.notify_generation();
        }

        let other = TcpStream::connect(r.addr).await.expect("connect");
        let (ord, mut owr) = other.into_split();
        let mut olines = BufReader::new(ord).lines();
        owr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let sub = timeout(Duration::from_secs(5), olines.next_line())
            .await
            .expect("a slow client must never stall the server")
            .expect("io")
            .expect("line");
        assert!(sub.contains("mining.notify"), "{sub}");
        drop(rd);
        r.srv.stop();
    }

    async fn rig_caps(cfg: ServerConfig, caps: Caps) -> Rig {
        rig_full_caps(cfg, caps, Arc::new(Mutex::new(E1Allocator::new()))).await
    }

    fn e1_of(subscribe_result: &str) -> E1 {
        let tail = subscribe_result
            .split("\"mining.set_target\"],\"")
            .nth(1)
            .expect("subscribe result shape");
        let hex = tail.split('"').next().expect("e1 field");
        E1(u32::from_str_radix(hex, 16).expect("e1 hex"))
    }

    fn notify_job_id(wire: &str) -> Option<u32> {
        let anchor = "\"mining.notify\",\"params\":[\"";
        let start = wire.find(anchor)? + anchor.len();
        u32::from_str_radix(wire.get(start..start + 8)?, 16).ok()
    }

    async fn handshake<R: AsyncBufReadExt + Unpin>(
        lines: &mut tokio::io::Lines<R>,
        wr: &mut OwnedWriteHalf,
    ) -> (E1, u32) {
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let sub = lines.next_line().await.expect("io").expect("line");
        let e1 = e1_of(&sub);
        wr.write_all(
            format!(
                "{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\n",
                login()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
        let mut job = None;
        for _ in 0..3 {
            if let Ok(Some(l)) = lines.next_line().await {
                if let Some(j) = notify_job_id(&l) {
                    job = Some(j);
                }
            }
        }
        (e1, job.expect("the handshake must end in a mining.notify"))
    }

    #[tokio::test]
    async fn banned_ip_dropped_at_accept() {
        let r = rig().await;
        {
            let mut b = r.srv.shared().bans.lock().expect("bans");

            assert!(b
                .penalise(
                    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                    crate::limits::BAN_THRESHOLD,
                    Severity::Hard,
                    0,
                )
                .is_some());
        }
        let before = Metrics::get(&r.srv.shared().metrics.refused_banned);
        let accepted_before = Metrics::get(&r.srv.shared().metrics.accepted);
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (mut rd, _wr) = stream.into_split();
        expect_closed(&mut rd, "a banned IP must not be seated by the accept loop").await;
        assert_eq!(
            Metrics::get(&r.srv.shared().metrics.refused_banned),
            before + 1,
            "the refusal must be counted in its own class"
        );
        assert_eq!(
            Metrics::get(&r.srv.shared().metrics.accepted),
            accepted_before,
            "a refused connection is not an accepted one"
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn per_ip_and_server_cap_distinct_classes() {
        let mut caps = Caps::for_mode(Mode::Pool);
        caps.max_per_ip = 1;
        let r = rig_caps(ServerConfig::for_mode(Mode::Pool), caps).await;
        let first = TcpStream::connect(r.addr).await.expect("connect");
        let (rd1, _w1) = first.into_split();

        for _ in 0..50 {
            if r.srv.live_connections() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let second = TcpStream::connect(r.addr).await.expect("connect");
        let (mut rd2, _w2) = second.into_split();
        expect_closed(&mut rd2, "a second connection from one IP must be refused").await;
        let m = &r.srv.shared().metrics;
        assert_eq!(Metrics::get(&m.refused_per_ip), 1, "wrong refusal class");
        assert_eq!(Metrics::get(&m.refused_full), 0, "wrong refusal class");
        assert_eq!(Metrics::get(&m.refused_rate), 0, "wrong refusal class");
        assert_eq!(Metrics::get(&m.refused_banned), 0, "wrong refusal class");
        drop(rd1);
        r.srv.stop();
    }

    #[tokio::test]
    async fn full_server_refuses_in_full_class() {
        let mut caps = Caps::for_mode(Mode::Pool);
        caps.max_connections = 1;
        let r = rig_caps(ServerConfig::for_mode(Mode::Pool), caps).await;
        let first = TcpStream::connect(r.addr).await.expect("connect");
        let (rd1, _w1) = first.into_split();
        for _ in 0..50 {
            if r.srv.live_connections() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let second = TcpStream::connect(r.addr).await.expect("connect");
        let (mut rd2, _w2) = second.into_split();
        expect_closed(&mut rd2, "a full server must refuse").await;
        let m = &r.srv.shared().metrics;
        assert_eq!(Metrics::get(&m.refused_full), 1, "wrong refusal class");
        assert_eq!(Metrics::get(&m.refused_per_ip), 0, "wrong refusal class");
        drop(rd1);
        r.srv.stop();
    }

    #[tokio::test]
    async fn flush_closes_past_output_cap() {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let a = l.local_addr().expect("addr");
        let cli = TcpStream::connect(a).await.expect("connect");
        let (_peer, _) = l.accept().await.expect("accept");
        let (_r, mut w) = cli.into_split();

        let mut out = vec![b'x'; OUT_BUF_CAP + 1];
        let verdict = flush(&mut w, &mut out, WRITE_TIMEOUT, OUT_BUF_CAP).await;
        assert!(
            matches!(verdict, Err(CloseReason::SlowClient)),
            "a buffer past OUT_BUF_CAP must close, not grow: {verdict:?}"
        );
        assert!(out.is_empty(), "the over-cap buffer must be dropped");

        let mut ok = vec![b'y'; OUT_BUF_CAP];
        assert!(
            flush(&mut w, &mut ok, WRITE_TIMEOUT, OUT_BUF_CAP).await.is_ok(),
            "the cap is a ceiling, not a fence one byte below itself"
        );
    }

    #[tokio::test]
    async fn stalled_write_closes_at_deadline() {
        // Fixed, small buffers on both ends. With the OS defaults the kernel is free to
        // grow them: Windows auto-tunes the loopback receive window, so under load the
        // "full" socket kept draining into the peer and the 64-byte flush below went
        // through (about two runs in three with the rest of this module in parallel).
        // Setting SO_SNDBUF/SO_RCVBUF explicitly turns that tuning off.
        const SOCK_BUF: u32 = 16 * 1024;
        let ls = TcpSocket::new_v4().expect("listener socket");
        ls.set_recv_buffer_size(SOCK_BUF).expect("listener rcvbuf");
        ls.bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
        let l = ls.listen(1).expect("listen");
        let a = l.local_addr().expect("addr");

        let cs = TcpSocket::new_v4().expect("client socket");
        cs.set_send_buffer_size(SOCK_BUF).expect("client sndbuf");
        let cli = cs.connect(a).await.expect("connect");

        let (_peer, _) = l.accept().await.expect("accept");
        let (_r, mut w) = cli.into_split();

        // Fill until the kernel refuses more, then keep checking for a while: a socket
        // is only stalled once WouldBlock holds with the other side quiet, not at the
        // first WouldBlock while buffered bytes are still moving to the peer.
        let block = [b'z'; 64 * 1024];
        let mut filled = 0usize;
        let mut quiet = 0;
        for _ in 0..4096 {
            match w.try_write(&block) {
                Ok(n) => {
                    filled += n;
                    quiet = 0;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    quiet += 1;
                    if quiet == 5 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("unexpected write error while filling: {e}"),
            }
        }
        assert!(filled > 0, "nothing was written; the rig is not in the state under test");
        assert_eq!(quiet, 5, "the socket never stopped accepting; the rig is not stalled");

        let deadline = Duration::from_millis(200);
        let mut out = vec![b'q'; 64];
        let t0 = Instant::now();
        let verdict = flush(&mut w, &mut out, deadline, OUT_BUF_CAP).await;
        let took = t0.elapsed();

        assert!(
            matches!(verdict, Err(CloseReason::SlowClient)),
            "a write that cannot complete must close the connection: {verdict:?}"
        );

        assert!(
            took >= deadline,
            "closed before its own deadline, in {took:?}"
        );
        assert!(
            took < Duration::from_secs(5),
            "closed in {took:?}, not at the configured {deadline:?}"
        );

        let drain = _peer;
        let mut sink = vec![0u8; 1 << 20];
        for _ in 0..64 {
            if drain.try_read(&mut sink).is_err() {
                break;
            }
        }
        let mut small = vec![b'k'; 8];
        assert!(
            flush(&mut w, &mut small, Duration::from_secs(5), OUT_BUF_CAP).await.is_ok(),
            "a socket that is being read must flush inside its deadline"
        );
        assert!(small.is_empty(), "a successful write must clear the buffer");
    }

    #[tokio::test]
    async fn flush_closes_when_the_write_itself_fails() {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let a = l.local_addr().expect("addr");
        let cli = TcpStream::connect(a).await.expect("connect");
        let (peer, _) = l.accept().await.expect("accept");
        drop(peer);
        let (_r, mut w) = cli.into_split();

        for _ in 0..64 {
            let mut out = vec![b'q'; 64];
            if let Err(e) = flush(&mut w, &mut out, WRITE_TIMEOUT, OUT_BUF_CAP).await {
                assert!(matches!(e, CloseReason::SlowClient), "{e:?}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("a write to a closed peer never reported an error");
    }

    #[tokio::test]
    async fn complete_oversize_line_refused() {
        let r = rig().await;
        let ip = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let before = {
            let mut b = r.srv.shared().bans.lock().expect("bans");
            b.score(ip, 0)
        };
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (mut rd, mut wr) = stream.into_split();

        wr.write_all(&vec![b'x'; MAX_LINE_PRE_AUTH])
            .await
            .expect("write");
        tokio::time::sleep(Duration::from_millis(50)).await;

        wr.write_all(b"abc\n").await.expect("write");
        expect_closed(&mut rd, "a complete line past the limit must be refused").await;

        assert_eq!(
            Metrics::get(&r.srv.shared().metrics.bad_json),
            1,
            "an oversize line is a bad message and must be counted as one"
        );
        let after = {
            let mut b = r.srv.shared().bans.lock().expect("bans");
            b.score(ip, 0)
        };
        assert_eq!(
            after - before,
            50,
            "docs/exceeding the line limit is a disconnect plus banscore +50"
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn newline_flood_after_auth_closed() {
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.tick_ms = 20;
        cfg.auth_deadline_ms = 30_000;
        cfg.idle_evict_ms = 30_000;
        cfg.read_deadline_ms = 30_000;
        let r = rig_with(cfg).await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = handshake(&mut lines, &mut wr).await;

        let flood = vec![b'\n'; 400];
        let _ = wr.write_all(&flood).await;
        expect_closed(
            lines.get_mut(),
            "a post-authorization newline flood must be charged and closed",
        )
        .await;
        r.srv.stop();
    }

    #[tokio::test]
    async fn tick_survives_fast_bytes() {
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.tick_ms = 200;
        cfg.auth_deadline_ms = 30_000;
        cfg.read_deadline_ms = 30_000;
        cfg.idle_evict_ms = 800;
        let r = rig_with(cfg).await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = handshake(&mut lines, &mut wr).await;

        let chatter = tokio::spawn(async move {
            loop {
                if wr
                    .write_all(b"{\"id\":3,\"method\":\"mining.configure\",\"params\":[]}\n")
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_millis(2_500);
        loop {
            match timeout(Duration::from_millis(100), lines.next_line()).await {
                Ok(Ok(None)) | Ok(Err(_)) => break,
                Ok(Ok(Some(_))) => {}
                Err(_) => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "bytes faster than the tick period held the tick off"
            );
        }
        chatter.abort();
        r.srv.stop();
    }

    #[tokio::test]
    async fn read_deadline_fires_at_deadline() {
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.tick_ms = 20;
        cfg.read_deadline_ms = 400;
        cfg.auth_deadline_ms = 30_000;
        cfg.idle_evict_ms = 30_000;
        let r = rig_with(cfg).await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (mut rd, mut wr) = stream.into_split();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let mut buf = [0u8; 512];
        let _ = timeout(Duration::from_millis(500), rd.read(&mut buf)).await;

        let t0 = std::time::Instant::now();

        match timeout(Duration::from_millis(160), rd.read(&mut buf)).await {
            Err(_) => {}
            Ok(Ok(0)) | Ok(Err(_)) => panic!(
                "closed after {:?}, far short of the {} ms read deadline",
                t0.elapsed(),
                400
            ),
            Ok(Ok(_)) => {}
        }

        expect_closed(&mut rd, "a socket that said nothing outlived the read deadline").await;
        let took = t0.elapsed();
        assert!(
            took < Duration::from_millis(640),
            "the read deadline is 400 ms and the socket survived {took:?}, \
             which is what a doubled bound looks like"
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn share_over_socket_gets_verdict() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let (e1, job_id) = handshake(&mut lines, &mut wr).await;

        let nonce = e1.compose(0x00_0000_0001);
        wr.write_all(
            format!(
                "{{\"id\":7,\"method\":\"mining.submit\",\"params\":[\"w\",\"{:08x}\",\"{}\"]}}\n",
                job_id,
                crate::nonce::nonce_to_hex(nonce)
            )
            .as_bytes(),
        )
        .await
        .expect("write");

        let answer = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("a submitted share must be answered")
            .expect("io")
            .expect("line");
        assert!(
            answer.contains("\"id\":7"),
            "the verdict must be addressed to the submit that caused it: {answer}"
        );
        assert!(
            Metrics::get(&r.srv.shared().metrics.shares_verified) >= 1,
            "the share never reached the interpreter"
        );
        r.srv.stop();
    }

    #[test]
    fn fatal_vs_transient_accept_error() {
        use io::ErrorKind::*;

        for k in [ConnectionAborted, ConnectionReset, Interrupted, WouldBlock, TimedOut] {
            assert!(
                !is_fatal_accept_error(&io::Error::from(k)),
                "{k:?} must be retried, not fatal: the server would die on one bad peer"
            );
        }

        for k in [InvalidInput, NotConnected, BrokenPipe] {
            assert!(
                is_fatal_accept_error(&io::Error::from(k)),
                "{k:?} must end the loop: retrying an unusable listener is a hot loop"
            );
        }
    }

    #[tokio::test]
    async fn stopped_server_seats_nobody() {
        let (srv, listener, addr) = unspawned(ServerConfig::for_mode(Mode::Pool)).await;
        srv.stop();
        let s2 = Arc::clone(&srv);
        let loop_done = tokio::spawn(async move { s2.serve(listener).await });

        let stream = TcpStream::connect(addr).await.expect("connect");
        let (mut rd, _wr) = stream.into_split();
        expect_closed(&mut rd, "a stopped server seated a connection").await;

        timeout(Duration::from_secs(5), loop_done)
            .await
            .expect("serve must return once it is stopped")
            .expect("join")
            .expect("serve");
        assert_eq!(
            Metrics::get(&srv.shared().metrics.accepted),
            0,
            "a stopped server counted an acceptance"
        );
        assert_eq!(srv.live_connections(), 0);
    }

    #[tokio::test]
    async fn seated_conn_counted_accepted() {
        let r = rig().await;
        assert_eq!(Metrics::get(&r.srv.shared().metrics.accepted), 0);
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = handshake(&mut lines, &mut wr).await;
        assert_eq!(Metrics::get(&r.srv.shared().metrics.accepted), 1);
        r.srv.stop();
    }

    #[tokio::test]
    async fn empty_outbuf_no_syscall() {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let a = l.local_addr().expect("addr");
        let cli = TcpStream::connect(a).await.expect("connect");
        let (peer, _) = l.accept().await.expect("accept");
        drop(peer);
        let (_r, mut w) = cli.into_split();

        let mut dead = false;
        for _ in 0..64 {
            let mut probe = vec![b'p'; 32];
            if flush(&mut w, &mut probe, WRITE_TIMEOUT, OUT_BUF_CAP).await.is_err() {
                dead = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(dead, "the peer never reported as gone; the rig is not in the state under test");
        let mut empty: Vec<u8> = Vec::new();
        assert!(
            flush(&mut w, &mut empty, WRITE_TIMEOUT, OUT_BUF_CAP).await.is_ok(),
            "an empty buffer must not be written to a dead socket, or to any socket"
        );
    }

    #[tokio::test]
    async fn two_crlf_messages_both_answered() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();

        let batch = format!(
            "\r\n{{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}}\r\n{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{}\",\"x\"]}}\r\n",
            login()
        );
        wr.write_all(batch.as_bytes()).await.expect("write");

        let mut saw_auth = false;
        for _ in 0..4 {
            let l = timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("the pipelined second message was never answered")
                .expect("io")
                .expect("line");
            if l.contains("\"id\":2") && l.contains("\"result\":true") {
                saw_auth = true;
                break;
            }
        }
        assert!(saw_auth, "the second message of a CRLF batch was lost");
        r.srv.stop();
    }

    #[tokio::test]
    async fn close_abandons_rest_of_batch() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();

        wr.write_all(b"zzz not json\n{\"id\":9,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");
        let mut seen = Vec::new();
        for _ in 0..4 {
            match timeout(Duration::from_secs(5), lines.next_line()).await {
                Ok(Ok(Some(l))) => seen.push(l),
                _ => break,
            }
        }
        assert!(
            !seen.iter().any(|l| l.contains("\"id\":9")),
            "a peer already being disconnected was still served its next message: {seen:?}"
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn oversize_reason_differs_by_phase() {
        let r = rig().await;
        let sh = Arc::clone(r.srv.shared());
        let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 9, 9, 1));

        let mut fresh = Session::new(1, ip, 0, &sh);
        assert!(!fresh.is_authorized());
        assert_eq!(oversize(&mut fresh, &sh, 0), CloseReason::GarbageBeforeAuth);

        let mut seated = Session::new(2, ip, 0, &sh);
        seated.on_line(
            br#"{"id":1,"method":"mining.subscribe","params":["t"]}"#,
            0,
            &sh,
        );
        seated.on_line(
            format!(
                r#"{{"id":2,"method":"mining.authorize","params":["{}","x"]}}"#,
                login()
            )
            .as_bytes(),
            0,
            &sh,
        );
        assert!(seated.is_authorized(), "the rig failed to seat the session");
        assert_eq!(
            oversize(&mut seated, &sh, 0),
            CloseReason::BadMessageAfterAuth
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn shutdown_closes_seated_conn() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = handshake(&mut lines, &mut wr).await;
        assert_eq!(r.srv.live_connections(), 1);

        r.srv.stop();
        let mut raw = lines.into_inner().into_inner();
        expect_closed(&mut raw, "a seated connection outlived stop()").await;
    }

    #[tokio::test]
    async fn new_tip_not_pushed_before_auth() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        wr.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"t\"]}\n")
            .await
            .expect("write");

        while let Ok(Ok(Some(_))) = timeout(Duration::from_millis(200), lines.next_line()).await {}

        r.src.new_tip(184_603);
        r.srv.notify_generation();

        let quiet = timeout(Duration::from_millis(500), lines.next_line()).await;
        assert!(
            quiet.is_err(),
            "an unauthorized connection was pushed work: {quiet:?}"
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn hangup_ends_task() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = handshake(&mut lines, &mut wr).await;
        assert_eq!(r.srv.live_connections(), 1);

        drop(wr);
        drop(lines);
        let t0 = Instant::now();
        while r.srv.live_connections() != 0 && t0.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            r.srv.live_connections(),
            0,
            "a hung-up peer left its task alive; `Ok(0)` is EOF, not an empty read"
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn zero_tick_does_not_kill_conn() {
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.tick_ms = 0;
        let r = rig_with(cfg).await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let (_e1, job) = timeout(Duration::from_secs(5), handshake(&mut lines, &mut wr))
            .await
            .expect("tick_ms = 0 killed the connection task");

        let _ = job;
        assert_eq!(r.srv.live_connections(), 1);
        r.srv.stop();
    }

    #[tokio::test]
    async fn reset_socket_ends_task() {
        let r = rig().await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");

        #[allow(deprecated)]
        stream.set_linger(Some(Duration::ZERO)).expect("set_linger(0) was refused by the OS");
        #[allow(deprecated)]
        let back = stream.linger().expect("linger() was refused by the OS");
        assert_eq!(
            back,
            Some(Duration::ZERO),
            "SO_LINGER did not stick, so the close below is a FIN not an RST"
        );
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = handshake(&mut lines, &mut wr).await;
        assert_eq!(r.srv.live_connections(), 1);

        r.src.new_tip(184_604);
        r.srv.notify_generation();
        tokio::time::sleep(Duration::from_millis(50)).await;

        wr.forget();
        drop(lines);
        let t0 = Instant::now();
        while r.srv.live_connections() != 0 && t0.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            r.srv.live_connections(),
            0,
            "a reset socket left its task alive; a read error must close"
        );
        r.srv.stop();
    }

    #[tokio::test]
    async fn stopped_reader_is_closed() {
        let mut cfg = ServerConfig::for_mode(Mode::Pool);
        cfg.write_timeout_ms = 200;

        cfg.tick_ms = 60_000;
        let r = rig_with(cfg).await;

        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let _ = handshake(&mut lines, &mut wr).await;
        assert_eq!(r.srv.live_connections(), 1);

        let held = lines.into_inner().into_inner();

        let t0 = Instant::now();
        let mut height = 184_603u64;
        while r.srv.live_connections() != 0 && t0.elapsed() < Duration::from_secs(20) {
            for _ in 0..64 {
                height += 1;
                r.src.new_tip(height);
                r.srv.notify_generation();
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            r.srv.live_connections(),
            0,
            "a connection that stopped reading was never closed"
        );
        drop(held);
        r.srv.stop();
    }

    struct FullVerifier;

    impl crate::verify::ShareVerifier for FullVerifier {
        fn enqueue(&self, _w: crate::verify::ShareWork) -> bool {
            false
        }
    }

    async fn rig_verifier(v: Arc<dyn crate::verify::ShareVerifier>) -> Rig {
        let src = Arc::new(MockJobSource::new(184_602, Target::from_difficulty(1 << 40)));
        let router = Arc::new(Router::new());
        let shared = Arc::new(Shared {
            bans: Mutex::new(crate::abuse::BanTable::new()),
            e1: Arc::new(Mutex::new(E1Allocator::new())),
            diffs: Mutex::new(crate::abuse::DiffCache::new()),
            accept: Mutex::new(crate::abuse::TokenBucket::new(500.0, 500.0, 0)),
            jobs: Arc::clone(&src) as Arc<dyn JobSource>,
            verifier: v,
            metrics: Metrics::default(),
            caps: Caps::for_mode(Mode::Pool),
            cfg: ServerConfig::for_mode(Mode::Pool),
        });
        let srv = StratumServer::new(shared, router);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let s2 = Arc::clone(&srv);
        tokio::spawn(async move {
            let _ = s2.serve(listener).await;
        });
        Rig { srv, src, addr }
    }

    #[tokio::test]
    async fn full_verifier_refunds_admission() {
        let r = rig_verifier(Arc::new(FullVerifier)).await;
        let stream = TcpStream::connect(r.addr).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        let (e1, job_id) = handshake(&mut lines, &mut wr).await;

        for i in 0..4u64 {
            let nonce = e1.compose(0x00_0000_0001 + i);
            wr.write_all(
                format!(
                    "{{\"id\":{},\"method\":\"mining.submit\",\"params\":[\"w\",\"{:08x}\",\"{}\"]}}\n",
                    100 + i,
                    job_id,
                    crate::nonce::nonce_to_hex(nonce)
                )
                .as_bytes(),
            )
            .await
            .expect("write");

            let answer = timeout(Duration::from_secs(5), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("submit {i} was never answered: the connection is wedged"))
                .expect("io")
                .expect("line");
            assert!(
                answer.contains("\"error\":[28,"),
                "submit {i} must be error 28 `server busy`, not {answer}"
            );
            assert!(
                answer.contains(&format!("\"id\":{}", 100 + i)),
                "the answer must be addressed to the submit that caused it: {answer}"
            );
        }
        assert_eq!(
            Metrics::get(&r.srv.shared().metrics.rej_server_busy),
            4,
            "every refusal must be counted in the server-busy class"
        );
        r.srv.stop();
    }
}
