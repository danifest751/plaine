#[path = "common/mod.rs"]
mod common;

use common::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

// account_getHistory against a real node, a real miner and a fresh chain: every
// coinbase the miner earned has to come back, newest first, pageable, and a node
// without the index has to say so instead of answering with an empty list.

fn rpc_raw(port: u16, method: &str, params: &str) -> String {
    let body = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{method}\",\"params\":{params}}}");
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

fn config_with_addrindex(dir: &Path, data: &Path, p2p: u16, rpc: u16, stratum: u16) -> PathBuf {
    let p = write_config(dir, data, p2p, rpc, stratum, &[]);
    let text = std::fs::read_to_string(&p).expect("read config");
    std::fs::write(&p, text.replacen("[node]\n", "[node]\naddrindex = true\n", 1)).expect("write config");
    p
}

fn count(json: &str, needle: &str) -> usize {
    json.matches(needle).count()
}

#[test]
fn history_lists_every_coinbase_the_miner_earned() {
    let dir = scratch("history");
    let data = dir.join("data");
    let cfg = config_with_addrindex(&dir, &data, 20_501, 20_502, 20_503);
    let n = start("history", &dir, &cfg, 20_501, 20_502, 20_503);
    wait_for_rpc(&n, Duration::from_secs(30));

    let h = mine_to(&n, 3, Duration::from_secs(300));
    assert!(h >= 3, "the miner produced only {h} blocks in 300 s");
    let h = height(n.rpc).expect("height");

    let all = rpc_raw(n.rpc, "account_getHistory", &format!("[\"{TEST_ADDRESS}\", 200]"));
    assert!(!all.contains("\"error\""), "{all}");
    // The harness mines to the address the genesis coinbase also pays, so the
    // history is every block 0..=h. Genesis carries no subsidy.
    assert_eq!(
        count(&all, "\"kind\":\"coinbase\""),
        h as usize + 1,
        "one coinbase per block 0..={h}: {all}"
    );
    assert_eq!(count(&all, "\"direction\":\"in\""), h as usize + 1, "{all}");
    assert_eq!(
        count(&all, "\"amountMile\":\"200000\""),
        h as usize,
        "every mined block credits 0.2 PLNE: {all}"
    );
    assert!(
        all.contains("\"height\":0,\"index\":0,") && all.contains("\"amountMile\":\"0\""),
        "the genesis coinbase is listed with the zero subsidy it carries: {all}"
    );
    assert!(all.contains("\"confirmations\":1,"), "the newest entry is the tip itself: {all}");
    assert!(all.contains("\"nextCursor\":null"), "{all}");
    assert!(all.contains("\"indexedFrom\":0"), "indexed from genesis: {all}");
    let newest = all.find(&format!("\"height\":{h},")).expect("the tip's coinbase is listed");
    let oldest = all.find("\"height\":0,").expect("the genesis coinbase is listed");
    assert!(newest < oldest, "newest first: {all}");

    // Walk it one entry at a time and get the same heights back.
    let mut cursor = "null".to_string();
    let mut walked = Vec::new();
    for _ in 0..=h {
        let page = rpc_raw(
            n.rpc,
            "account_getHistory",
            &format!("[\"{TEST_ADDRESS}\", 1, {cursor}]"),
        );
        assert!(!page.contains("\"error\""), "{page}");
        let got = num(&page, "height").expect("each page holds one entry");
        walked.push(got);
        match text(&page, "nextCursor") {
            Some(c) => cursor = format!("\"{c}\""),
            None => break,
        }
    }
    let want: Vec<u64> = (0..=h).rev().collect();
    assert_eq!(walked, want, "paging one by one must visit every coinbase once, in order");
}

#[test]
fn a_node_without_the_index_refuses_rather_than_answering_empty() {
    let dir = scratch("history-off");
    let data = dir.join("data");
    let cfg = write_config(&dir, &data, 20_511, 20_512, 20_513, &[]);
    let n = start("history-off", &dir, &cfg, 20_511, 20_512, 20_513);
    wait_for_rpc(&n, Duration::from_secs(30));

    let got = rpc_raw(n.rpc, "account_getHistory", &format!("[\"{TEST_ADDRESS}\"]"));
    assert!(got.contains("\"error\""), "{got}");
    assert!(got.contains("addrindex = true"), "the error must name the switch: {got}");
}

#[test]
fn the_index_is_announced_at_startup() {
    let dir = scratch("history-log");
    let data = dir.join("data");
    let cfg = config_with_addrindex(&dir, &data, 20_521, 20_522, 20_523);
    let n = start("history-log", &dir, &cfg, 20_521, 20_522, 20_523);
    wait_for_rpc(&n, Duration::from_secs(30));
    let log = std::fs::read_to_string(&n.log).unwrap_or_default();
    assert!(log.contains("addrindex on"), "the operator must see the index is on:\n{log}");
}
