// The parts of the wallet below the screens: the HTTP client against a real socket,
// the send checks, the sent log and what it can tell without a history, settings.

mod common;

use plaine_wallet_gui::app::{passphrase_problem, suggest_passphrase};
use plaine_wallet_gui::model::{
    append_sent, plan_send, read_sent, sent_status, FeeLevel, SendForm, SentRecord, SentStatus,
    Settings,
};
use plaine_wallet_gui::rpc::{self, Account, Fees, HttpNode, RpcError, Transport};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;

/// Answers one HTTP request with `response`, and hands back what was asked.
fn one_shot(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let h = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap();
        s.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    });
    (addr, h)
}

#[test]
fn the_http_client_speaks_json_rpc_the_way_the_node_expects() {
    let (addr, h) = one_shot(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
         {\"jsonrpc\":\"2.0\",\"result\":{\"height\":7},\"id\":1}",
    );
    let node = HttpNode::new(addr, Some("t0k3n".into()));
    let v = node.call("chain_getInfo", json!([])).unwrap();
    assert_eq!(v["height"], 7);
    let req = h.join().unwrap();
    assert!(req.starts_with("POST / HTTP/1.1\r\n"), "{req}");
    assert!(
        req.contains("Content-Type: application/json\r\n"),
        "the node answers 415 without it"
    );
    assert!(req.contains("Authorization: Bearer t0k3n\r\n"), "{req}");
    assert!(req.contains("\"method\":\"chain_getInfo\""), "{req}");
}

#[test]
fn node_errors_keep_their_code_and_detail() {
    let (addr, _h) = one_shot(
        "HTTP/1.1 200 OK\r\n\r\n{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32601,\
         \"message\":\"Method not found\",\"data\":{\"detail\":\"no such method\"}},\"id\":1}",
    );
    let e = HttpNode::new(addr, None)
        .call("account_getHistory", json!([]))
        .unwrap_err();
    assert_eq!(
        e,
        RpcError::Rpc {
            code: -32601,
            message: "Method not found".into(),
            detail: Some("no such method".into())
        }
    );
    assert!(e.is_unsupported(), "an upstream node without the method");

    let (addr, _h) = one_shot("HTTP/1.1 401 Unauthorized\r\n\r\n{\"error\":\"Unauthorized\"}");
    let e = HttpNode::new(addr, None)
        .call("chain_getInfo", json!([]))
        .unwrap_err();
    assert!(matches!(e, RpcError::Http { status: 401, .. }));
    assert!(e.describe().contains("token"), "{}", e.describe());

    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string();
    let e = HttpNode::new(closed, None)
        .call("chain_getInfo", json!([]))
        .unwrap_err();
    assert!(matches!(e, RpcError::Transport(_)), "{e:?}");
}

#[test]
fn an_upstream_node_reads_as_history_unsupported_not_as_a_failure() {
    let node = common::MockNode::upstream();
    match rpc::history(&node, "plne1x", 10, None).unwrap() {
        rpc::History::Unsupported(why) => assert!(why.contains("Method not found"), "{why}"),
        other => panic!("{other:?}"),
    }
}

fn account(spendable: u128, nonce: u64, pending_nonce: u64) -> Account {
    Account {
        balance: spendable,
        nonce,
        pending_nonce,
        immature: 0,
        spendable,
    }
}

const FEES: Fees = Fees {
    blocks_sampled: 240,
    low: 1,
    normal: 10,
    high: 250,
    floor: 5,
};

#[test]
fn a_send_plan_uses_the_pending_nonce_and_never_a_fee_below_the_floor() {
    let form = SendForm {
        to: " plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5 ".into(),
        amount: "0.5".into(),
        level: FeeLevel::Low,
    };
    let plan = plan_send(&form, Some(&account(1_000_000, 4, 6)), Some(&FEES)).unwrap();
    assert_eq!(plan.to, "plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5");
    assert_eq!((plan.amount, plan.fee, plan.nonce), (500_000, 5, 6));

    let all = SendForm {
        amount: "1".into(),
        ..form.clone()
    };
    let e = plan_send(&all, Some(&account(1_000_000, 0, 0)), Some(&FEES)).unwrap_err();
    assert!(
        e.amount.unwrap().contains("more than"),
        "amount plus fee must fit"
    );

    let e = plan_send(&form, None, None).unwrap_err();
    assert!(e.other.unwrap().contains("waiting for the node"));
}

#[test]
fn the_sent_log_tells_status_without_a_history() {
    let dir = common::scratch("sentlog");
    let log = dir.join("k.plnekey.sent");
    let r = |nonce: u64, txid: &str| SentRecord {
        txid: txid.into(),
        nonce,
        amount: 10,
        fee: 1,
        to: "plne1x".into(),
        time: 1,
    };
    append_sent(&log, &r(3, "aa")).unwrap();
    append_sent(&log, &r(4, "bb")).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"garbage line\n")
        .unwrap();
    let got = read_sent(&log);
    assert_eq!(
        got,
        vec![r(3, "aa"), r(4, "bb")],
        "a bad line is skipped, not fatal"
    );

    let a = account(0, 4, 5);
    assert_eq!(
        sent_status(&got[0], &a, &[]),
        SentStatus::Confirmed,
        "nonce moved past it"
    );
    assert_eq!(
        sent_status(&got[1], &a, &["bb".into()]),
        SentStatus::Pending
    );
    assert_eq!(
        sent_status(&got[1], &a, &[]),
        SentStatus::NotIncluded,
        "gone, nonce not past it"
    );
}

#[test]
fn settings_survive_a_round_trip() {
    let s = Settings {
        node: "10.0.0.2:9257".into(),
        token: "x".repeat(40),
        key_file: "C:/keys/w.plnekey".into(),
        lock_after_minutes: 3,
        miner: "D:/tools/plaine-miner.exe".into(),
        stratum: "10.0.0.2:9258".into(),
        rig: "desk".into(),
    };
    assert_eq!(Settings::parse(&s.render()), s);
    assert_eq!(
        Settings::parse("nonsense\nnode=\n"),
        Settings::default(),
        "empty values keep defaults"
    );
}

#[test]
fn suggested_passphrases_are_long_and_different() {
    let a = suggest_passphrase().unwrap();
    let b = suggest_passphrase().unwrap();
    assert_eq!(a.len(), 23, "20 characters in groups of five: {a}");
    assert_ne!(a, b);
    assert_eq!(passphrase_problem(&a, &a), None);
    assert!(passphrase_problem("short", "short").is_some());
    assert!(passphrase_problem("long enough passphrase", "long enough passphrasf").is_some());
}

#[test]
fn times_are_shown_in_utc_calendar_form() {
    use plaine_wallet_gui::model::format_utc;
    assert_eq!(format_utc(0), "1970-01-01 00:00");
    assert_eq!(format_utc(951_782_400), "2000-02-29 00:00", "a leap day");
    assert_eq!(format_utc(1_790_167_525), "2026-09-23 12:45");
}

#[test]
fn the_qr_codes_uppercase_address_is_still_a_valid_address() {
    // The QR code carries the address in capitals (alphanumeric mode, a smaller
    // code); whatever scans it must be able to paste it back.
    let a = "plne10srlmaehj5mvh42gn7freevqjrkd8e8ervaet5";
    assert!(plaine_wallet::api::check_address(&a.to_uppercase()).is_ok());
    assert!(qrcode::QrCode::new(a.to_uppercase().as_bytes()).is_ok());
}
