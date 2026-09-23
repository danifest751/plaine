//! The node, as the wallet sees it: a few JSON-RPC methods over HTTP to a local
//! `plaine-noded`. Everything goes through [`Transport`], so tests put a mock in
//! the node's place, including one shaped like upstream's node, which has no
//! address history.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// What went wrong talking to the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RpcError {
    /// No connection, or it broke: the node is not running, or not at that address.
    Transport(String),
    /// An HTTP refusal before JSON-RPC: a wrong token (401), a busy node (503).
    Http { status: u16, body: String },
    /// A JSON-RPC error object.
    Rpc {
        code: i64,
        message: String,
        detail: Option<String>,
    },
    /// An answer this wallet cannot read.
    Shape(String),
}

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const FEATURE_DISABLED: i64 = -32003;
pub const NOT_FOUND: i64 = -32001;
pub const TX_REJECTED: i64 = -32002;

impl RpcError {
    /// The node cannot answer this at all: an older node without the method, or
    /// one that runs without the index it needs.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, RpcError::Rpc { code, .. } if *code == METHOD_NOT_FOUND || *code == FEATURE_DISABLED)
    }

    /// One sentence for a person.
    pub fn describe(&self) -> String {
        match self {
            RpcError::Transport(e) => format!("cannot reach the node: {e}"),
            RpcError::Http { status: 401, .. } => {
                "the node refused the RPC token; check it in Settings".to_string()
            }
            RpcError::Http { status, body } => format!("the node answered HTTP {status}: {body}"),
            RpcError::Rpc {
                message,
                detail: Some(d),
                ..
            } => format!("{message}: {d}"),
            RpcError::Rpc {
                message,
                detail: None,
                ..
            } => message.clone(),
            RpcError::Shape(e) => format!("unexpected answer from the node: {e}"),
        }
    }
}

/// Carries one JSON-RPC call to a node and brings back its `result`.
pub trait Transport: Send + Sync {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

/// A node reached over HTTP/1.1, one connection per call.
#[derive(Clone, Debug)]
pub struct HttpNode {
    pub address: String,
    pub token: Option<String>,
    pub timeout: Duration,
}

impl HttpNode {
    pub fn new(address: impl Into<String>, token: Option<String>) -> HttpNode {
        HttpNode {
            address: address.into(),
            token,
            timeout: Duration::from_secs(10),
        }
    }

    fn post(&self, body: &str) -> Result<(u16, String), RpcError> {
        let t = |e: std::io::Error| RpcError::Transport(format!("{} ({e})", self.address));
        let addr = self
            .address
            .to_socket_addrs()
            .map_err(t)?
            .next()
            .ok_or_else(|| RpcError::Transport(format!("{} resolves to nothing", self.address)))?;
        let mut s = TcpStream::connect_timeout(&addr, self.timeout).map_err(t)?;
        s.set_read_timeout(Some(self.timeout)).map_err(t)?;
        s.set_write_timeout(Some(self.timeout)).map_err(t)?;
        let auth = match &self.token {
            Some(tok) => format!("Authorization: Bearer {tok}\r\n"),
            None => String::new(),
        };
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n{auth}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.address,
            body.len()
        );
        s.write_all(req.as_bytes()).map_err(t)?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).map_err(t)?;
        let text = String::from_utf8_lossy(&raw);
        let (head, payload) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| RpcError::Shape("no HTTP header end".into()))?;
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| RpcError::Shape(format!("bad status line {:?}", head.lines().next())))?;
        Ok((status, payload.to_string()))
    }
}

impl Transport for HttpNode {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let (status, payload) = self.post(&body.to_string())?;
        if status != 200 {
            return Err(RpcError::Http {
                status,
                body: payload.trim().to_string(),
            });
        }
        let v: Value = serde_json::from_str(&payload)
            .map_err(|e| RpcError::Shape(format!("{e}: {payload}")))?;
        if let Some(e) = v.get("error") {
            return Err(RpcError::Rpc {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                detail: e
                    .get("data")
                    .and_then(|d| d.get("detail"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| RpcError::Shape("no result".into()))
    }
}

fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value, RpcError> {
    v.get(name)
        .ok_or_else(|| RpcError::Shape(format!("no `{name}` in {v}")))
}

fn u64_of(v: &Value, name: &str) -> Result<u64, RpcError> {
    field(v, name)?
        .as_u64()
        .ok_or_else(|| RpcError::Shape(format!("`{name}` is not an integer")))
}

fn str_of(v: &Value, name: &str) -> Result<String, RpcError> {
    Ok(field(v, name)?
        .as_str()
        .ok_or_else(|| RpcError::Shape(format!("`{name}` is not a string")))?
        .to_string())
}

/// Amounts arrive as decimal strings of mile.
fn mile_of(v: &Value, name: &str) -> Result<u128, RpcError> {
    str_of(v, name)?
        .parse()
        .map_err(|_| RpcError::Shape(format!("`{name}` is not an amount")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainInfo {
    pub height: u64,
    pub sync: String,
    pub peers: u64,
    pub tip_age_secs: u64,
    pub best_known_height: Option<u64>,
}

pub fn chain_info(t: &dyn Transport) -> Result<ChainInfo, RpcError> {
    let v = t.call("chain_getInfo", json!([]))?;
    Ok(ChainInfo {
        height: u64_of(&v, "height")?,
        sync: str_of(&v, "sync")?,
        peers: u64_of(&v, "peers")?,
        tip_age_secs: u64_of(&v, "tipAgeSecs")?,
        best_known_height: v.get("bestKnownHeight").and_then(Value::as_u64),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub balance: u128,
    pub nonce: u64,
    pub pending_nonce: u64,
    pub immature: u128,
    pub spendable: u128,
}

pub fn account(t: &dyn Transport, address: &str) -> Result<Account, RpcError> {
    let v = t.call("account_get", json!([address]))?;
    Ok(Account {
        balance: mile_of(&v, "balance")?,
        nonce: u64_of(&v, "nonce")?,
        pending_nonce: u64_of(&v, "pendingNonce")?,
        immature: mile_of(&v, "immature")?,
        spendable: mile_of(&v, "spendable")?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub txid: String,
    pub height: u64,
    pub time: u64,
    pub confirmations: u64,
    /// `coinbase`, `transfer` or `announcement`.
    pub kind: String,
    /// `in`, `out` or `self`.
    pub direction: String,
    pub amount: u128,
    pub fee: u128,
    pub counterparty: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next_cursor: Option<String>,
    pub hint: Option<String>,
}

/// A page of history, or why this node cannot give one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum History {
    Page(HistoryPage),
    Unsupported(String),
}

pub fn history(
    t: &dyn Transport,
    address: &str,
    limit: u32,
    cursor: Option<&str>,
) -> Result<History, RpcError> {
    let v = match t.call("account_getHistory", json!([address, limit, cursor])) {
        Ok(v) => v,
        Err(e) if e.is_unsupported() => return Ok(History::Unsupported(e.describe())),
        Err(e) => return Err(e),
    };
    let mut entries = Vec::new();
    for e in field(&v, "entries")?
        .as_array()
        .ok_or_else(|| RpcError::Shape("entries".into()))?
    {
        entries.push(HistoryEntry {
            txid: str_of(e, "txid")?,
            height: u64_of(e, "height")?,
            time: u64_of(e, "time")?,
            confirmations: u64_of(e, "confirmations")?,
            kind: str_of(e, "kind")?,
            direction: str_of(e, "direction")?,
            amount: mile_of(e, "amountMile")?,
            fee: mile_of(e, "feeMile")?,
            counterparty: e
                .get("counterparty")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(History::Page(HistoryPage {
        entries,
        next_cursor: v
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_string),
        hint: v.get("hint").and_then(Value::as_str).map(str::to_string),
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fees {
    pub blocks_sampled: u64,
    pub low: u128,
    pub normal: u128,
    pub high: u128,
    pub floor: u128,
}

pub fn fees(t: &dyn Transport) -> Result<Fees, RpcError> {
    let v = t.call("fee_suggest", json!([]))?;
    Ok(Fees {
        blocks_sampled: u64_of(&v, "blocksSampled")?,
        low: mile_of(&v, "p10Mile")?,
        normal: mile_of(&v, "p50Mile")?,
        high: mile_of(&v, "p90Mile")?,
        floor: mile_of(&v, "relayFloorMile")?,
    })
}

/// The ids of this address's transactions waiting in the mempool.
pub fn pending(t: &dyn Transport, address: &str) -> Result<Vec<String>, RpcError> {
    let v = t.call("mempool_getBySender", json!([address]))?;
    let list = v
        .as_array()
        .ok_or_else(|| RpcError::Shape("not an array".into()))?;
    list.iter().map(|tx| str_of(tx, "txid")).collect()
}

/// Submits a signed transaction; returns its id.
pub fn send_raw(t: &dyn Transport, hex: &str) -> Result<String, RpcError> {
    t.call("tx_sendRaw", json!([hex]))?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| RpcError::Shape("tx_sendRaw did not return a txid".into()))
}
