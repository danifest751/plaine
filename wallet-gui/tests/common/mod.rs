#![allow(dead_code)]

use plaine_wallet_gui::rpc::{RpcError, Transport};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A node in memory. With `history: None` it answers `account_getHistory` the
/// way upstream's node does: the method does not exist.
#[derive(Clone)]
pub struct MockNode {
    pub state: Arc<Mutex<MockState>>,
}

pub struct MockState {
    pub height: u64,
    pub spendable: u128,
    pub immature: u128,
    pub nonce: u64,
    pub fees: (u128, u128, u128, u128),
    pub history: Option<Vec<Value>>,
    pub pending: Vec<String>,
    /// Every raw transaction submitted, as hex.
    pub submitted: Vec<String>,
    pub reject_sends: Option<String>,
    pub calls: Vec<String>,
}

impl MockNode {
    pub fn new(history: Option<Vec<Value>>) -> MockNode {
        MockNode {
            state: Arc::new(Mutex::new(MockState {
                height: 1_234,
                spendable: 5_000_000,
                immature: 400_000,
                nonce: 3,
                fees: (1, 10, 250, 1),
                history,
                pending: Vec::new(),
                submitted: Vec::new(),
                reject_sends: None,
                calls: Vec::new(),
            })),
        }
    }

    pub fn upstream() -> MockNode {
        MockNode::new(None)
    }

    pub fn with_history(entries: Vec<Value>) -> MockNode {
        MockNode::new(Some(entries))
    }
}

impl Transport for MockNode {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(method.to_string());
        match method {
            "chain_getInfo" => Ok(json!({"height": s.height, "sync": "synced", "peers": 8,
                "tipAgeSecs": 12, "bestKnownHeight": s.height})),
            "account_get" => Ok(json!({
                "balance": (s.spendable + s.immature).to_string(),
                "nonce": s.nonce,
                "pendingNonce": s.nonce + s.pending.len() as u64,
                "immature": s.immature.to_string(),
                "spendable": s.spendable.to_string(),
            })),
            "fee_suggest" => Ok(
                json!({"blocksSampled": 240, "p10Mile": s.fees.0.to_string(),
                "p50Mile": s.fees.1.to_string(), "p90Mile": s.fees.2.to_string(),
                "relayFloorMile": s.fees.3.to_string()}),
            ),
            "mempool_getBySender" => Ok(Value::Array(
                s.pending
                    .iter()
                    .map(|t| json!({"txid": t, "type": 1}))
                    .collect(),
            )),
            "account_getHistory" => match &s.history {
                None => Err(RpcError::Rpc {
                    code: -32601,
                    message: "Method not found".into(),
                    detail: Some("no method \"account_getHistory\"".into()),
                }),
                Some(entries) => {
                    let limit = params[1].as_u64().unwrap_or(50) as usize;
                    let start = params[2]
                        .as_str()
                        .map(|c| c.parse::<usize>().unwrap())
                        .unwrap_or(0);
                    let page: Vec<Value> =
                        entries.iter().skip(start).take(limit).cloned().collect();
                    let next = (start + limit < entries.len()).then(|| (start + limit).to_string());
                    Ok(
                        json!({"address": params[0], "indexedFrom": 0, "entries": page, "nextCursor": next}),
                    )
                }
            },
            "tx_sendRaw" => {
                if let Some(why) = &s.reject_sends {
                    return Err(RpcError::Rpc {
                        code: -32002,
                        message: "Transaction rejected".into(),
                        detail: Some(why.clone()),
                    });
                }
                let hex = params[0].as_str().unwrap().to_string();
                let raw = plaine_consensus::hex::decode(&hex).unwrap();
                let tx = plaine_consensus::codec::TransferTx::decode(&raw).unwrap();
                let txid = plaine_consensus::hex::encode(&tx.txid());
                s.submitted.push(hex);
                s.pending.push(txid.clone());
                Ok(json!(txid))
            }
            other => Err(RpcError::Rpc {
                code: -32601,
                message: format!("no {other}"),
                detail: None,
            }),
        }
    }
}

pub fn entry(
    txid: &str,
    height: u64,
    direction: &str,
    kind: &str,
    amount: u128,
    fee: u128,
) -> Value {
    json!({"txid": txid, "height": height, "index": 0, "time": 1_790_000_000u64 + height,
        "confirmations": 1_235 - height, "kind": kind, "direction": direction,
        "amountMile": amount.to_string(), "feeMile": fee.to_string(),
        "counterparty": if kind == "transfer" { json!("plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5") } else { Value::Null }})
}

pub fn scratch(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "plaine-gui-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}
