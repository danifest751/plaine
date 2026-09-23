//! State and logic of the wallet, without egui: what the node last said, the send
//! flow, the history as shown, and the log of sent transfers. The UI reads these
//! and never talks to the node itself.

use crate::rpc::{self, Account, ChainInfo, Fees, History, HistoryEntry, Transport};
use plaine_wallet::api;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the node is asked again while the wallet is open.
pub const REFRESH_EVERY: Duration = Duration::from_secs(10);

/// Entries per history page.
pub const HISTORY_PAGE: u32 = 50;

/// Whether the node can list this address's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryState {
    /// Not asked yet.
    Unknown,
    /// Confirmed entries so far, newest first, and where the next page starts.
    Listed {
        entries: Vec<HistoryEntry>,
        next_cursor: Option<String>,
        hint: Option<String>,
    },
    /// The node has no `account_getHistory` (an upstream node) or runs without
    /// `addrindex`; the reason is the node's own words.
    Unsupported(String),
}

/// What the node last said. Each part keeps its own error, so a failed history
/// call does not blank the balance.
#[derive(Clone, Debug)]
pub struct NodeView {
    pub chain: Option<Result<ChainInfo, String>>,
    pub account: Option<Result<Account, String>>,
    pub fees: Option<Fees>,
    pub pending: Vec<String>,
    pub history: HistoryState,
    pub history_error: Option<String>,
    /// The outcome of the last submission: a txid or why it was refused.
    pub last_send: Option<Result<String, String>>,
    pub refreshes: u64,
}

impl Default for NodeView {
    fn default() -> Self {
        NodeView {
            chain: None,
            account: None,
            fees: None,
            pending: Vec::new(),
            history: HistoryState::Unknown,
            history_error: None,
            last_send: None,
            refreshes: 0,
        }
    }
}

impl NodeView {
    pub fn account(&self) -> Option<&Account> {
        self.account.as_ref().and_then(|a| a.as_ref().ok())
    }

    pub fn node_error(&self) -> Option<&str> {
        match &self.chain {
            Some(Err(e)) => Some(e),
            _ => None,
        }
    }
}

/// Work for the node.
#[derive(Clone, Debug)]
pub enum Cmd {
    /// Ask again for everything, the first history page included.
    Refresh,
    /// The next history page.
    MoreHistory,
    /// Submit a signed transfer (hex), then refresh.
    Submit { hex: String, record: SentRecord },
}

/// Runs node calls away from the UI and publishes what they return. In the
/// desktop wallet that is a thread; in tests it runs inline, so a click is
/// answered before the next frame.
pub struct Worker {
    view: Arc<Mutex<NodeView>>,
    mode: Mode,
}

enum Mode {
    Thread(Sender<Cmd>),
    Inline(Box<dyn Transport>, String, Option<PathBuf>),
}

impl Worker {
    /// Starts a thread that serves commands and refreshes every [`REFRESH_EVERY`].
    /// `wake` is called whenever the view changes, to repaint the UI.
    pub fn spawn(
        node: Box<dyn Transport>,
        address: String,
        sent_log: Option<PathBuf>,
        wake: impl Fn() + Send + 'static,
    ) -> Worker {
        let view = Arc::new(Mutex::new(NodeView::default()));
        let (tx, rx): (Sender<Cmd>, Receiver<Cmd>) = channel();
        let shared = Arc::clone(&view);
        std::thread::spawn(move || {
            let mut cmd = Some(Cmd::Refresh);
            loop {
                if let Some(c) = cmd.take() {
                    serve(&*node, &address, sent_log.as_deref(), &shared, c);
                    wake();
                }
                match rx.recv_timeout(REFRESH_EVERY) {
                    Ok(c) => cmd = Some(c),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => cmd = Some(Cmd::Refresh),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Worker {
            view,
            mode: Mode::Thread(tx),
        }
    }

    /// Serves every command at once on the caller's thread.
    pub fn inline(node: Box<dyn Transport>, address: String, sent_log: Option<PathBuf>) -> Worker {
        let w = Worker {
            view: Arc::new(Mutex::new(NodeView::default())),
            mode: Mode::Inline(node, address, sent_log),
        };
        w.send(Cmd::Refresh);
        w
    }

    pub fn send(&self, cmd: Cmd) {
        match &self.mode {
            Mode::Thread(tx) => {
                let _ = tx.send(cmd);
            }
            Mode::Inline(node, address, log) => {
                serve(&**node, address, log.as_deref(), &self.view, cmd)
            }
        }
    }

    /// A copy of what the node last said.
    pub fn view(&self) -> NodeView {
        self.view.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

fn serve(
    node: &dyn Transport,
    address: &str,
    log: Option<&Path>,
    view: &Mutex<NodeView>,
    cmd: Cmd,
) {
    match cmd {
        Cmd::Refresh => {
            let chain = rpc::chain_info(node).map_err(|e| e.describe());
            let reachable = chain.is_ok();
            let (account, fees, pending, history) = if reachable {
                (
                    Some(rpc::account(node, address).map_err(|e| e.describe())),
                    rpc::fees(node).ok(),
                    rpc::pending(node, address).unwrap_or_default(),
                    Some(rpc::history(node, address, HISTORY_PAGE, None)),
                )
            } else {
                (None, None, Vec::new(), None)
            };
            let Ok(mut v) = view.lock() else { return };
            v.chain = Some(chain);
            if let Some(a) = account {
                v.account = Some(a);
            }
            if fees.is_some() {
                v.fees = fees;
            }
            v.pending = pending;
            match history {
                Some(Ok(h)) => {
                    v.history = history_state(h);
                    v.history_error = None;
                }
                Some(Err(e)) => v.history_error = Some(e.describe()),
                None => {}
            }
            v.refreshes += 1;
        }
        Cmd::MoreHistory => {
            let cursor = match view.lock().map(|v| v.history.clone()) {
                Ok(HistoryState::Listed {
                    next_cursor: Some(c),
                    ..
                }) => c,
                _ => return,
            };
            let page = rpc::history(node, address, HISTORY_PAGE, Some(&cursor));
            let Ok(mut v) = view.lock() else { return };
            match page {
                Ok(History::Page(p)) => {
                    if let HistoryState::Listed {
                        entries,
                        next_cursor,
                        hint,
                    } = &mut v.history
                    {
                        entries.extend(p.entries);
                        *next_cursor = p.next_cursor;
                        if p.hint.is_some() {
                            *hint = p.hint;
                        }
                    }
                }
                Ok(History::Unsupported(r)) => v.history = HistoryState::Unsupported(r),
                Err(e) => v.history_error = Some(e.describe()),
            }
        }
        Cmd::Submit { hex, record } => {
            let outcome = rpc::send_raw(node, &hex).map_err(|e| e.describe());
            if let (Ok(_), Some(path)) = (&outcome, log) {
                if let Err(e) = append_sent(path, &record) {
                    if let Ok(mut v) = view.lock() {
                        v.last_send = Some(Err(format!(
                            "sent as {}, but the local log of sent transfers could not be \
                             written: {e}",
                            record.txid
                        )));
                    }
                    serve(node, address, log, view, Cmd::Refresh);
                    return;
                }
            }
            if let Ok(mut v) = view.lock() {
                v.last_send = Some(outcome);
            }
            serve(node, address, log, view, Cmd::Refresh);
        }
    }
}

fn history_state(h: History) -> HistoryState {
    match h {
        History::Page(p) => HistoryState::Listed {
            entries: p.entries,
            next_cursor: p.next_cursor,
            hint: p.hint,
        },
        History::Unsupported(r) => HistoryState::Unsupported(r),
    }
}

/// The fee a sender picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeLevel {
    Low,
    Normal,
    High,
}

impl FeeLevel {
    pub fn pick(&self, f: &Fees) -> u128 {
        match self {
            FeeLevel::Low => f.low,
            FeeLevel::Normal => f.normal,
            FeeLevel::High => f.high,
        }
        .max(f.floor)
    }
}

/// The send form as typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendForm {
    pub to: String,
    pub amount: String,
    pub level: FeeLevel,
}

impl Default for SendForm {
    fn default() -> Self {
        SendForm {
            to: String::new(),
            amount: String::new(),
            level: FeeLevel::Normal,
        }
    }
}

/// A checked transfer, ready to be signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendPlan {
    pub to: String,
    pub amount: u128,
    pub fee: u128,
    pub nonce: u64,
}

impl SendPlan {
    pub fn total(&self) -> u128 {
        self.amount + self.fee
    }
}

/// What is wrong with the form, field by field, for a person to fix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormErrors {
    pub to: Option<String>,
    pub amount: Option<String>,
    pub other: Option<String>,
}

impl FormErrors {
    pub fn is_empty(&self) -> bool {
        self.to.is_none() && self.amount.is_none() && self.other.is_none()
    }
}

/// Checks the form against what the node said. The nonce is the node's
/// `pendingNonce`: after everything this address already has in the mempool.
pub fn plan_send(
    form: &SendForm,
    account: Option<&Account>,
    fees: Option<&Fees>,
) -> Result<SendPlan, FormErrors> {
    let mut e = FormErrors::default();
    let to = form.to.trim();
    if to.is_empty() {
        e.to = Some("enter the recipient's address".into());
    } else if let Err(err) = api::check_address(to) {
        e.to = Some(err.to_string().replace("format: ", ""));
    }
    let amount = match api::parse_plne(&form.amount) {
        Ok(0) => {
            e.amount = Some("the amount must be more than zero".into());
            None
        }
        Ok(a) => Some(a),
        Err(err) => {
            e.amount = Some(err.to_string().replace("usage: ", ""));
            None
        }
    };
    let (Some(account), Some(fees)) = (account, fees) else {
        e.other = Some("waiting for the node: balance and fees are not known yet".into());
        return Err(e);
    };
    let fee = form.level.pick(fees);
    if let Some(a) = amount {
        if a.saturating_add(fee) > account.spendable {
            e.amount = Some(format!(
                "{} PLNE plus the {} PLNE fee is more than the {} PLNE you can spend",
                api::format_plne(a),
                api::format_plne(fee),
                api::format_plne(account.spendable)
            ));
        }
    }
    match (e.is_empty(), amount) {
        (true, Some(amount)) => Ok(SendPlan {
            to: to.to_string(),
            amount,
            fee,
            nonce: account.pending_nonce,
        }),
        _ => Err(e),
    }
}

/// One line of the local log of transfers this wallet sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentRecord {
    pub txid: String,
    pub nonce: u64,
    pub amount: u128,
    pub fee: u128,
    pub to: String,
    pub time: u64,
}

/// The sent log beside a key file: `<key file>.sent`.
pub fn sent_log_for(key: &Path) -> PathBuf {
    let mut p = key.as_os_str().to_owned();
    p.push(".sent");
    PathBuf::from(p)
}

pub fn append_sent(path: &Path, r: &SentRecord) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        f,
        "{} {} {} {} {} {}",
        r.txid, r.nonce, r.amount, r.fee, r.to, r.time
    )?;
    f.sync_all()
}

/// Reads the sent log; lines that do not parse are skipped, not fatal.
pub fn read_sent(path: &Path) -> Vec<SentRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            if p.len() != 6 {
                return None;
            }
            Some(SentRecord {
                txid: p[0].to_string(),
                nonce: p[1].parse().ok()?,
                amount: p[2].parse().ok()?,
                fee: p[3].parse().ok()?,
                to: p[4].to_string(),
                time: p[5].parse().ok()?,
            })
        })
        .collect()
}

/// Where a sent transfer stands, from the node's account and mempool alone:
/// what an upstream node, which has no history, can still tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SentStatus {
    Pending,
    /// The account nonce moved past it and it left the mempool.
    Confirmed,
    /// Not in the mempool, and the nonce has not moved past it: dropped or
    /// replaced. The funds were not spent by it.
    NotIncluded,
}

pub fn sent_status(r: &SentRecord, account: &Account, pending: &[String]) -> SentStatus {
    if pending.iter().any(|t| t == &r.txid) {
        SentStatus::Pending
    } else if r.nonce < account.nonce {
        SentStatus::Confirmed
    } else {
        SentStatus::NotIncluded
    }
}

/// A row of the history as shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub txid: String,
    /// When: the block's time, or when this wallet sent it; empty if unknown.
    pub when: String,
    /// "pending", or the confirmations.
    pub status: String,
    pub direction: String,
    pub kind: String,
    /// Signed, in PLNE: "+0.2", "-1.5".
    pub amount: String,
    pub counterparty: String,
}

/// The history to show: this address's transactions still in the mempool first,
/// then the confirmed ones. Pending ones the sent log knows get their details;
/// others (sent by another copy of this key) show the txid only.
pub fn rows(view: &NodeView, sent: &[SentRecord]) -> Vec<Row> {
    let mut out: Vec<Row> = view
        .pending
        .iter()
        .map(|txid| match sent.iter().find(|r| &r.txid == txid) {
            Some(r) => Row {
                txid: txid.clone(),
                when: format_utc(r.time),
                status: "pending".into(),
                direction: "out".into(),
                kind: "transfer".into(),
                amount: format!("-{}", api::format_plne(r.amount + r.fee)),
                counterparty: r.to.clone(),
            },
            None => Row {
                txid: txid.clone(),
                when: String::new(),
                status: "pending".into(),
                direction: "out".into(),
                kind: "transfer".into(),
                amount: String::new(),
                counterparty: String::new(),
            },
        })
        .collect();
    match &view.history {
        HistoryState::Listed { entries, .. } => {
            out.extend(entries.iter().map(|e| {
                let signed = match e.direction.as_str() {
                    "in" => format!("+{}", api::format_plne(e.amount)),
                    "self" => format!("-{}", api::format_plne(e.fee)),
                    _ => format!("-{}", api::format_plne(e.amount + e.fee)),
                };
                Row {
                    txid: e.txid.clone(),
                    when: format_utc(e.time),
                    status: format!("{} conf.", e.confirmations),
                    direction: e.direction.clone(),
                    kind: e.kind.clone(),
                    amount: signed,
                    counterparty: e.counterparty.clone().unwrap_or_default(),
                }
            }));
        }
        HistoryState::Unsupported(_) | HistoryState::Unknown => {
            // No history from the node: what this wallet sent, from its own log.
            if let Some(a) = view.account() {
                let mut own: Vec<&SentRecord> = sent
                    .iter()
                    .filter(|r| !view.pending.contains(&r.txid))
                    .collect();
                own.sort_by_key(|r| std::cmp::Reverse(r.nonce));
                out.extend(own.into_iter().map(|r| Row {
                    txid: r.txid.clone(),
                    when: format_utc(r.time),
                    status: match sent_status(r, a, &view.pending) {
                        SentStatus::Confirmed => "confirmed".into(),
                        SentStatus::Pending => "pending".into(),
                        SentStatus::NotIncluded => "not included".into(),
                    },
                    direction: "out".into(),
                    kind: "transfer".into(),
                    amount: format!("-{}", api::format_plne(r.amount + r.fee)),
                    counterparty: r.to.clone(),
                }));
            }
        }
    }
    out
}

/// Settings kept between runs, in a small `key=value` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub node: String,
    pub token: String,
    pub key_file: String,
    pub lock_after_minutes: u64,
    /// The miner program; empty means `plaine-miner` beside the wallet.
    pub miner: String,
    /// Where the miner gets work; empty means the node's host, port 9258.
    pub stratum: String,
    /// The worker name shown by the node or pool.
    pub rig: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            node: "127.0.0.1:9257".into(),
            token: String::new(),
            key_file: String::new(),
            lock_after_minutes: 10,
            miner: String::new(),
            stratum: String::new(),
            rig: "wallet".into(),
        }
    }
}

impl Settings {
    pub fn render(&self) -> String {
        format!(
            "node={}\ntoken={}\nkey_file={}\nlock_after_minutes={}\nminer={}\nstratum={}\nrig={}\n",
            self.node,
            self.token,
            self.key_file,
            self.lock_after_minutes,
            self.miner,
            self.stratum,
            self.rig
        )
    }

    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().to_string();
            match k.trim() {
                "node" if !v.is_empty() => s.node = v,
                "token" => s.token = v,
                "key_file" => s.key_file = v,
                "miner" => s.miner = v,
                "stratum" => s.stratum = v,
                "rig" => s.rig = v,
                "lock_after_minutes" => {
                    if let Ok(n) = v.parse() {
                        s.lock_after_minutes = n;
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path)
            .map(|t| Settings::parse(&t))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.render())
    }

    pub fn transport(&self) -> rpc::HttpNode {
        let token = (!self.token.trim().is_empty()).then(|| self.token.trim().to_string());
        rpc::HttpNode::new(self.node.trim(), token)
    }
}

/// The wallet's own directory: `%APPDATA%\Plaine` on Windows, the node's; else
/// `~/.config/plaine`.
pub fn config_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("Plaine");
    }
    match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h).join(".config").join("plaine"),
        None => PathBuf::from("."),
    }
}

/// Unix seconds as `YYYY-MM-DD HH:MM` UTC.
pub fn format_utc(secs: u64) -> String {
    // Howard Hinnant's days-to-civil, for the proleptic Gregorian calendar.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3_600,
        rem % 3_600 / 60
    )
}
