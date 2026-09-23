#[path = "common/mod.rs"]
mod common;

use common::*;
use plaine_consensus::constants::{Network, COINBASE_MATURITY, FEE_FLOOR_MILE};
use plaine_consensus::crypto::address_from_pubkey;
use plaine_consensus::hex;
use plaine_wallet::secret::Secret32;
use plaine_wallet::{sig, txbuild};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

// One transfer followed through a real node the way a wallet follows it: signed
// with plaine-wallet, submitted, seen in the mempool, confirmed, listed in both
// parties' histories, and found by id again after a restart, on a node that
// keeps the address index but no txid index.
//
// The sender has to own a mature coinbase first, so the test mines past
// COINBASE_MATURITY: a few minutes of real proof of work. It is ignored by
// default; `scripts/check.sh --e2e` runs it.

const P2P: u16 = 20_531;
const RPC: u16 = 20_532;
const STRATUM: u16 = 20_533;

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

fn stop(n: &mut Node) {
    request_stop(n).ok();
    if wait_exit(n, Duration::from_secs(30)).is_none() {
        hard_kill(n);
    }
}

/// The history entry for `txid`, as the raw JSON object text.
fn history_entry(port: u16, address: &str, txid: &str) -> String {
    let page = rpc_raw(port, "account_getHistory", &format!("[\"{address}\", 20]"));
    assert!(!page.contains("\"error\""), "{page}");
    let at = page
        .find(&format!("\"txid\":\"{txid}\""))
        .unwrap_or_else(|| panic!("{txid} is not in {address}'s history: {page}"));
    let end = page[at..].find('}').expect("entry end") + at;
    page[at..end].to_string()
}

#[test]
#[ignore = "mines past coinbase maturity, minutes of real proof of work; scripts/check.sh --e2e runs it"]
fn a_transfer_is_followed_from_the_mempool_into_both_histories() {
    ensure_console();
    let seed = Secret32::from_bytes([0x5a; 32]);
    let from = address_from_pubkey(&sig::public_key_of(&seed));
    let to = address_from_pubkey(&sig::public_key_of(&Secret32::from_bytes([0xa5; 32])));
    let stranger = address_from_pubkey(&sig::public_key_of(&Secret32::from_bytes([0x3c; 32])));

    let dir = scratch("transfer");
    let data = dir.join("data");
    let cfg = write_config(&dir, &data, P2P, RPC, STRATUM, &[]);
    let text_cfg = std::fs::read_to_string(&cfg).expect("read config");
    std::fs::write(&cfg, text_cfg.replacen("[node]\n", "[node]\naddrindex = true\n", 1))
        .expect("write config");
    let mut n = start("transfer", &dir, &cfg, P2P, RPC, STRATUM);
    wait_for_rpc(&n, Duration::from_secs(30));

    // The coinbase of block 1 matures at 1 + COINBASE_MATURITY.
    let want = COINBASE_MATURITY + 2;
    let h = mine_to_address(&n, &from, want, Duration::from_secs(1_200));
    assert!(h >= want, "the miner reached only {h} of {want} blocks in 20 minutes");

    let acct = rpc_raw(RPC, "account_get", &format!("[\"{from}\"]"));
    let spendable: u128 = text(&acct, "spendable").and_then(|s| s.parse().ok()).expect(&acct);
    assert!(spendable >= 200_000, "a matured coinbase must be spendable: {acct}");
    let nonce = num(&acct, "nonce").expect("nonce");

    let amount: u128 = 150_000;
    let tx = txbuild::build_transfer(Network::Main, &seed, &to, amount, FEE_FLOOR_MILE, nonce)
        .expect("build the transfer");
    let txid = hex::encode(&tx.txid());
    let sent = rpc_raw(RPC, "tx_sendRaw", &format!("[\"{}\"]", hex::encode(&tx.encode())));
    assert!(sent.contains(&format!("\"result\":\"{txid}\"")), "tx_sendRaw: {sent}");

    // Pending: visible by sender and by id, no index involved.
    let pending = rpc_raw(RPC, "mempool_getBySender", &format!("[\"{from}\"]"));
    assert!(pending.contains(&txid), "{pending}");
    let got = rpc_raw(RPC, "tx_get", &format!("[\"{txid}\"]"));
    assert!(got.contains("\"where\":\"mempool\""), "{got}");

    // Confirmed: mine until the sender's mempool is empty.
    let top = mine_to_address(&n, &from, h + 2, Duration::from_secs(300));
    assert!(top >= h + 2, "the miner stalled at {top}");
    let pending = rpc_raw(RPC, "mempool_getBySender", &format!("[\"{from}\"]"));
    assert!(!pending.contains(&txid), "still pending after two blocks: {pending}");

    let out = history_entry(RPC, &from, &txid);
    assert!(out.contains("\"kind\":\"transfer\""), "{out}");
    assert!(out.contains("\"direction\":\"out\""), "{out}");
    assert!(out.contains(&format!("\"amountMile\":\"{amount}\"")), "{out}");
    assert!(out.contains(&format!("\"feeMile\":\"{FEE_FLOOR_MILE}\"")), "{out}");
    assert!(out.contains(&format!("\"counterparty\":\"{to}\"")), "{out}");
    let inn = history_entry(RPC, &to, &txid);
    assert!(inn.contains("\"direction\":\"in\""), "{inn}");
    assert!(inn.contains(&format!("\"counterparty\":\"{from}\"")), "{inn}");
    let paid = rpc_raw(RPC, "account_get", &format!("[\"{to}\"]"));
    assert_eq!(text(&paid, "balance").as_deref(), Some("150000"), "{paid}");

    // After a restart nothing is held in memory: the id alone is not enough
    // without txindex, and an address the transaction touches is.
    stop(&mut n);
    let mut n = start("transfer-again", &dir, &cfg, P2P, RPC, STRATUM);
    wait_for_rpc(&n, Duration::from_secs(30));

    let bare = rpc_raw(RPC, "tx_get", &format!("[\"{txid}\"]"));
    assert!(bare.contains("\"error\"") && bare.contains("addrindex"), "{bare}");
    for side in [&from, &to] {
        let found = rpc_raw(RPC, "tx_get", &format!("[\"{txid}\", \"{side}\"]"));
        assert!(found.contains("\"where\":\"block\""), "via {side}: {found}");
        assert!(found.contains(&hex::encode(&tx.encode())), "the raw bytes come back: {found}");
    }
    let miss = rpc_raw(RPC, "tx_get", &format!("[\"{txid}\", \"{stranger}\"]"));
    assert!(miss.contains("touch that address"), "an uninvolved address finds nothing: {miss}");

    stop(&mut n);
}
