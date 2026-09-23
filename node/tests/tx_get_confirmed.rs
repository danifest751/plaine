#[path = "common/mod.rs"]
mod common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

fn rpc_raw(port: u16, method: &str, params: &str) -> String {
    let body = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{method}\",\"params\":{params}}}"
    );
    let req = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect to rpc");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.write_all(req.as_bytes()).expect("write");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.split_once("\r\n\r\n").map(|(_, j)| j.to_string()).unwrap_or(out)
}

const P2P: u16 = 34_356;
const RPC: u16 = 34_357;
const STRATUM: u16 = 34_359;
const P2P_NOINDEX: u16 = 34_366;
const RPC_NOINDEX: u16 = 34_367;
const STRATUM_NOINDEX: u16 = 34_369;

fn write_config_txindex(dir: &Path, data: &Path, p2p: u16, rpc: u16, stratum: u16, txindex: bool) -> std::path::PathBuf {
    let text = format!(
        "[node]\nnetwork = \"main\"\ndata_dir = {:?}\nprune = false\ntxindex = {txindex}\n\n\
         [p2p]\nlisten = \"127.0.0.1:{p2p}\"\nuse_embedded_seeds = false\n\n\
         [rpc]\nlisten = \"127.0.0.1:{rpc}\"\n\n\
         [stratum]\nlisten = \"127.0.0.1:{stratum}\"\n",
        data.display().to_string().replace('\\', "/")
    );
    let p = dir.join(format!("noded-{rpc}.toml"));
    std::fs::write(&p, text).expect("write config");
    p
}

fn first_txid_at(rpc_port: u16, height: u64) -> String {
    let blk = rpc(rpc_port, "chain_getBlockByHeight", &format!("[{height},1]"))
        .unwrap_or_else(|| panic!("chain_getBlockByHeight [{height}] answered nothing"));
    let at = blk.find("\"txids\"").unwrap_or_else(|| panic!("no txids in {blk}"));
    let rest = &blk[at..];
    let open = rest.find('"').and_then(|_| rest.find('[')).expect("txids array");
    let inner = &rest[open + 1..];
    let end = inner.find(']').expect("txids array end");
    let first = inner[..end]
        .split(',')
        .next()
        .expect("at least one txid")
        .trim()
        .trim_matches('"')
        .to_string();
    assert_eq!(first.len(), 64, "a txid is 32 bytes of hex, got {first:?}");
    first
}

#[test]
fn tx_get_answers_for_mined_tx() {
    ensure_console();
    if miner_binary().is_none() {
        eprintln!("SKIP: plaine-miner could not be built; this proof needs real proof of work");
        return;
    }
    let dir = scratch("txget");
    let data = dir.join("data");
    let cfg = write_config_txindex(&dir, &data, P2P, RPC, STRATUM, true);
    let mut node = start("indexed", &dir, &cfg, P2P, RPC, STRATUM);

    let h = mine_to(&node, 4, Duration::from_secs(300));
    assert!(h >= 3, "the miner produced only {h} blocks; nothing to look up");

    let txid = first_txid_at(RPC, 2);

    let got = rpc(RPC, "tx_get", &format!("[\"{txid}\"]")).unwrap_or_else(|| {
        panic!(
            "tx_get [{txid}] failed on a node with txindex = true that holds the block. \
             Node log:\n{}",
            std::fs::read_to_string(&node.log).unwrap_or_default()
        )
    });
    assert!(
        got.contains("\"where\":\"block\"") || got.contains("\"where\": \"block\""),
        "tx_get answered without placing the transaction in a block: {got}"
    );
    assert_eq!(
        num(&got, "height"),
        Some(2),
        "tx_get placed the transaction at the wrong height: {got}"
    );
    let conf = num(&got, "confirmations").expect("confirmations");
    assert!(
        conf >= 1 && conf <= h + 1,
        "confirmations {conf} is not a depth on a chain of height {h}: {got}"
    );
    assert!(
        got.contains(&txid),
        "tx_get answered about a different transaction: {got}"
    );

    let nowhere = "00".repeat(32);
    let miss = rpc_raw(RPC, "tx_get", &format!("[\"{nowhere}\"]"));
    assert!(
        miss.contains("no transaction with that id"),
        "an indexed node must say plainly that an unknown id is unknown: {miss}"
    );

    let audit = rpc(RPC, "emission_audit", &format!("[{h}]"))
        .unwrap_or_else(|| panic!("emission_audit [{h}] answered nothing"));
    assert!(
        audit.contains("\"matchesFormula\":true") || audit.contains("\"matchesFormula\": true"),
        "a healthy young chain must audit clean: {audit}"
    );
    assert!(
        audit.contains("\"underMaxSupply\":true") || audit.contains("\"underMaxSupply\": true"),
        "{audit}"
    );

    request_stop(&node).ok();
    if wait_exit(&mut node, Duration::from_secs(30)).is_none() {
        hard_kill(&mut node);
    }
}

#[test]
fn node_without_index_refuses() {
    ensure_console();
    let dir = scratch("txget-noindex");
    let data = dir.join("data");
    let cfg = write_config_txindex(&dir, &data, P2P_NOINDEX, RPC_NOINDEX, STRATUM_NOINDEX, false);
    let mut node = start("unindexed", &dir, &cfg, P2P_NOINDEX, RPC_NOINDEX, STRATUM_NOINDEX);

    let id = "11".repeat(32);
    let out = rpc_raw(RPC_NOINDEX, "tx_get", &format!("[\"{id}\"]"));
    assert!(
        out.contains("no txid index"),
        "a node with txindex off must say it cannot look, not that the id does not exist: {out}"
    );
    assert!(
        !out.contains("no transaction with that id"),
        "\"no transaction with that id\" from a node that never looked is the defect this \
         file exists for: {out}"
    );

    request_stop(&node).ok();
    if wait_exit(&mut node, Duration::from_secs(30)).is_none() {
        hard_kill(&mut node);
    }
}
