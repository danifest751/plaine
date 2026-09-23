#[path = "common/mod.rs"]
mod common;

use common::*;
use plaine_consensus::checkpoint_record;
use plaine_consensus::ed25519_dalek::{Signer, SigningKey};
use plaine_consensus::rules::{checkpoint_message, CheckpointSig, SignedCheckpoint};
use std::path::Path;
use std::time::Duration;

const P2P: u16 = 34_256;
const RPC: u16 = 34_257;
const STRATUM: u16 = 34_259;

fn authority() -> SigningKey {
    SigningKey::from_bytes(&[0x3A; 32])
}

fn record(sk: &SigningKey, height: u64, hash: [u8; 32]) -> String {
    let sig = sk.sign(&checkpoint_message(height, &hash)).to_bytes();
    let cp = SignedCheckpoint {
        height,
        hash,
        sigs: vec![CheckpointSig { pubkey: sk.verifying_key().to_bytes(), sig }],
    };
    plaine_consensus::hex::encode(&checkpoint_record::encode(&cp))
}

fn write_config_with_key(dir: &Path, data: &Path, pubkey_hex: &str) -> std::path::PathBuf {
    let text = format!(
        "[node]\nnetwork = \"main\"\ndata_dir = {:?}\n\n\
         [p2p]\nlisten = \"127.0.0.1:{P2P}\"\nseeds = []\nuse_embedded_seeds = false\n\n\
         [rpc]\nlisten = \"127.0.0.1:{RPC}\"\n\n\
         [stratum]\nlisten = \"127.0.0.1:{STRATUM}\"\n\n\
         [checkpoints]\nenabled = true\nthreshold = 1\nkeys = [\"{pubkey_hex}\"]\n",
        data.display().to_string().replace('\\', "/")
    );
    let p = dir.join("noded.toml");
    std::fs::write(&p, text).expect("write config");
    p
}

fn raw_rpc(port: u16, method: &str, params: &str) -> String {
    use std::io::{Read, Write};
    let body =
        format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{method}\",\"params\":{params}}}");
    let req = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("rpc connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    s.write_all(req.as_bytes()).expect("rpc write");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.split_once("\r\n\r\n").map(|(_, j)| j.to_string()).unwrap_or(out)
}

fn status(port: u16) -> String {
    raw_rpc(port, "checkpoint_getStatus", "[]")
}

#[test]
fn anchor_survives_hard_kill() {
    let dir = scratch("cp-anchor-restart");
    let data = dir.join("data");
    let sk = authority();
    let pubkey = plaine_consensus::hex::encode(&sk.verifying_key().to_bytes());
    let cfg = write_config_with_key(&dir, &data, &pubkey);

    let mut n = start("first", &dir, &cfg, P2P, RPC, STRATUM);
    let s = status(RPC);
    assert!(
        s.contains("\"lastAnchor\":null"),
        "a fresh node must hold no anchor; got {s}"
    );
    assert_eq!(
        text(&s, "keySource").as_deref(),
        Some("config"),
        "the test key must be the one in force, or this proves nothing about it: {s}"
    );
    assert_eq!(
        text(&s, "ingest").as_deref(),
        Some("live"),
        "a node that cannot receive a checkpoint cannot be asked to keep one: {s}"
    );

    let hash = [0x11u8; 32];
    let rec = record(&sk, 5_000, hash);
    let r = raw_rpc(RPC, "checkpoint_submit", &format!("[\"{rec}\"]"));
    assert!(r.contains("\"result\":\"advanced\""), "submit was refused: {r}");
    assert!(r.contains("\"anchorAdvanced\":true"), "{r}");
    assert!(
        r.contains("\"enforcing\":false"),
        "the node holds no block at 5000, so nothing may be enforced: {r}"
    );

    let s = status(RPC);
    assert_eq!(num(&s, "height"), Some(5_000), "the anchor is not live: {s}");

    hard_kill(&mut n);
    let n2 = start("second", &dir, &cfg, P2P, RPC, STRATUM);

    let s = status(RPC);
    assert_eq!(
        num(&s, "height"),
        Some(5_000),
        "THE ANCHOR DID NOT SURVIVE THE RESTART. Either nothing was persisted \
         (`Sink::put_anchor` is a no-op again), or what was persisted could not be \
         re-verified on load. Status was: {s}"
    );
    assert!(
        s.contains(&plaine_consensus::hex::encode(&hash)),
        "the reloaded anchor names a different block: {s}"
    );

    let r = raw_rpc(RPC, "checkpoint_submit", &format!("[\"{rec}\"]"));
    assert!(r.contains("\"result\":\"unchanged\""), "a replay claimed an advance: {r}");
    assert!(r.contains("\"anchorAdvanced\":false"), "{r}");

    let mut n2 = n2;
    hard_kill(&mut n2);
}

#[test]
fn wrong_key_record_refused() {
    let dir = scratch("cp-anchor-wrongkey");
    let data = dir.join("data");
    let sk = authority();
    let pubkey = plaine_consensus::hex::encode(&sk.verifying_key().to_bytes());
    let cfg = write_config_with_key(&dir, &data, &pubkey);

    let (p2p, rpc_port, stratum) = (P2P + 10, RPC + 10, STRATUM + 10);
    let text_cfg = std::fs::read_to_string(&cfg)
        .expect("config")
        .replace(&P2P.to_string(), &p2p.to_string())
        .replace(&RPC.to_string(), &rpc_port.to_string())
        .replace(&STRATUM.to_string(), &stratum.to_string());
    std::fs::write(&cfg, text_cfg).expect("rewrite config");

    let mut n = start("wrongkey", &dir, &cfg, p2p, rpc_port, stratum);

    let good = record(&sk, 4_000, [0x22u8; 32]);
    let r = raw_rpc(rpc_port, "checkpoint_submit", &format!("[\"{good}\"]"));
    assert!(r.contains("\"result\":\"advanced\""), "{r}");

    let other = SigningKey::from_bytes(&[0x9Au8; 32]);
    let forged = record(&other, 9_000, [0x33u8; 32]);
    let r = raw_rpc(rpc_port, "checkpoint_submit", &format!("[\"{forged}\"]"));
    assert!(r.contains("\"error\""), "an unsigned-by-us record was accepted: {r}");
    assert!(r.contains("\"reason\":\"unverified\""), "{r}");

    let genesis = record(&sk, 0, [0x44u8; 32]);
    let r = raw_rpc(rpc_port, "checkpoint_submit", &format!("[\"{genesis}\"]"));
    assert!(r.contains("\"reason\":\"genesisImmutable\""), "{r}");

    let r = raw_rpc(rpc_port, "checkpoint_submit", "[\"00010203\"]");
    assert!(r.contains("\"reason\":\"malformed\""), "{r}");

    let s = status(rpc_port);
    assert_eq!(num(&s, "height"), Some(4_000), "a refused record moved the anchor: {s}");
    hard_kill(&mut n);
    let mut n = start("wrongkey2", &dir, &cfg, p2p, rpc_port, stratum);
    let s = status(rpc_port);
    assert_eq!(num(&s, "height"), Some(4_000), "the anchor changed across a restart: {s}");
    hard_kill(&mut n);
}
