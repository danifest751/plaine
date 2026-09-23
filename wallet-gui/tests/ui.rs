// The wallet's screens, driven headless through egui_kittest against a node in
// memory: first run, unlocking, sending, history, and a node shaped like
// upstream's, which has no address history.

mod common;

use common::*;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use plaine_consensus::constants::Network;
use plaine_wallet::api::{self, Kdf};
use plaine_wallet::keyfile::Role as KeyRole;
use plaine_wallet::secret::SecretBytes;
use plaine_wallet_gui::app::{Connect, WalletApp};
use plaine_wallet_gui::model::{read_sent, sent_log_for, Settings};
use plaine_wallet_gui::rpc::Transport;
use std::path::Path;
use std::sync::Arc;

const RECIPIENT: &str = "plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5";

fn harness(node: &MockNode, key_file: &Path) -> Harness<'static, WalletApp> {
    harness_sized(node, key_file, 900.0)
}

fn harness_sized(node: &MockNode, key_file: &Path, height: f32) -> Harness<'static, WalletApp> {
    plaine_wallet_gui::kdf::install();
    let node = node.clone();
    let connect: Connect =
        Arc::new(move |_s: &Settings| Box::new(node.clone()) as Box<dyn Transport>);
    let settings = Settings {
        key_file: key_file.display().to_string(),
        ..Settings::default()
    };
    let app = WalletApp::for_tests(settings, connect);
    let mut h = Harness::builder()
        .with_size([900.0, height])
        .build_ui_state(|ui, app: &mut WalletApp| app.show(ui), app);
    h.run();
    h
}

fn fill(h: &mut Harness<'static, WalletApp>, label: &str, text: &str) {
    let node = match h.query_by_role_and_label(Role::TextInput, label) {
        Some(n) => n,
        None => h.get_by_role_and_label(Role::PasswordInput, label),
    };
    node.focus();
    node.type_text(text);
    h.run();
}

fn has_input(h: &Harness<'static, WalletApp>, label: &str) -> bool {
    h.query_by_role_and_label(Role::TextInput, label).is_some()
        || h.query_by_role_and_label(Role::PasswordInput, label)
            .is_some()
}

fn click(h: &mut Harness<'static, WalletApp>, label: &str) {
    h.get_by_label(label).click();
    h.run();
}

/// A key file without a passphrase, fast to open.
fn plain_key(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("plain.plnekey");
    api::create_key(
        &path,
        KeyRole::Spend,
        None,
        None,
        Kdf::Blake3Iter { iters: 0 },
    )
    .unwrap();
    path
}

#[test]
fn first_run_creates_an_argon2id_key_and_shows_the_backup_once() {
    let dir = scratch("create");
    let path = dir.join("new.plnekey");
    let node = MockNode::upstream();
    let mut h = harness(&node, &path);

    click(&mut h, "Create a new key");
    fill(&mut h, "Passphrase", "a long enough passphrase");
    fill(&mut h, "Repeat passphrase", "a long enough passphrase");
    click(&mut h, "Create");

    assert!(h.state().is_open());
    h.get_by_label_contains("Write down this backup string now");
    assert_eq!(api::inspect(&path).unwrap().kdf, "argon2id-v1");
    click(&mut h, "I have written it down");
    assert!(
        h.query_by_label_contains("Write down this backup")
            .is_none(),
        "shown once"
    );
    h.get_by_label("Your address");
    h.get_by_label("5.0 PLNE");
}

#[test]
fn a_short_or_mismatched_passphrase_is_refused_before_anything_is_written() {
    let dir = scratch("weak");
    let path = dir.join("new.plnekey");
    let mut h = harness(&MockNode::upstream(), &path);
    click(&mut h, "Create a new key");
    fill(&mut h, "Passphrase", "short");
    fill(&mut h, "Repeat passphrase", "short");
    click(&mut h, "Create");
    h.get_by_label_contains("at least 12 characters");
    assert!(!path.exists());
}

#[test]
fn an_encrypted_key_asks_for_its_passphrase_and_locks_again() {
    plaine_wallet_gui::kdf::install();
    let dir = scratch("unlock");
    let path = dir.join("k.plnekey");
    let pass = SecretBytes::from_vec(b"correct horse battery".to_vec());
    api::create_key(&path, KeyRole::Spend, None, Some(&pass), Kdf::RECOMMENDED).unwrap();
    let mut h = harness(&MockNode::upstream(), &path);

    click(&mut h, "Open");
    h.get_by_role_and_label(Role::Button, "Unlock");
    fill(&mut h, "Passphrase", "wrong horse battery");
    h.get_by_role_and_label(Role::Button, "Unlock").click();
    h.run();
    h.get_by_label_contains("MAC mismatch");
    assert!(!h.state().is_open());

    fill(&mut h, "Passphrase", "correct horse battery");
    h.get_by_role_and_label(Role::Button, "Unlock").click();
    h.run();
    assert!(h.state().is_open());

    click(&mut h, "Lock");
    assert!(!h.state().is_open(), "locking drops the key");
    assert!(has_input(&h, "Passphrase"), "back at the passphrase prompt");
}

#[test]
fn an_unencrypted_key_opens_directly_with_a_warning() {
    let dir = scratch("plain");
    let path = plain_key(&dir);
    let mut h = harness(&MockNode::upstream(), &path);
    click(&mut h, "Open");
    assert!(h.state().is_open());
    h.get_by_label_contains("This key file is not encrypted");
}

#[test]
fn sending_signs_what_was_confirmed_and_logs_it() {
    let dir = scratch("send");
    let path = plain_key(&dir);
    let node = MockNode::upstream();
    let mut h = harness(&node, &path);
    click(&mut h, "Open");
    click(&mut h, "Send");

    fill(&mut h, "Recipient address", RECIPIENT);
    fill(&mut h, "Amount (PLNE)", "1.5");
    click(&mut h, "High (0.00025 PLNE)");
    click(&mut h, "Review");
    h.get_by_label("Confirm");
    h.get_by_label("1.50025 PLNE");
    click(&mut h, "Sign and send");

    let submitted = node.state.lock().unwrap().submitted.clone();
    assert_eq!(submitted.len(), 1);
    let raw = plaine_consensus::hex::decode(&submitted[0]).unwrap();
    let tx = plaine_consensus::codec::TransferTx::decode(&raw).unwrap();
    plaine_consensus::crypto::verify_transfer_signature(Network::Main, &tx).unwrap();
    assert_eq!(
        (tx.amount, tx.fee, tx.nonce),
        (1_500_000, 250, 3),
        "pendingNonce from the node"
    );
    assert_eq!(
        plaine_consensus::crypto::address_from_pubkey(&tx.from_pub),
        api::inspect(&path).unwrap().address
    );
    h.get_by_label_contains("Sent: ");

    let log = read_sent(&sent_log_for(&path));
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].txid, plaine_consensus::hex::encode(&tx.txid()));
}

#[test]
fn the_form_says_what_is_wrong_and_sends_nothing() {
    let dir = scratch("form");
    let path = plain_key(&dir);
    let node = MockNode::upstream();
    let mut h = harness(&node, &path);
    click(&mut h, "Open");
    click(&mut h, "Send");

    fill(
        &mut h,
        "Recipient address",
        "plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp4",
    );
    fill(&mut h, "Amount (PLNE)", "99");
    click(&mut h, "Review");
    h.get_by_label_contains("not a valid Plaine address");
    h.get_by_label_contains("more than the 5.0 PLNE you can spend");
    assert!(h.query_by_label("Confirm").is_none());
    assert!(node.state.lock().unwrap().submitted.is_empty());
}

#[test]
fn a_refused_transfer_is_reported_in_the_nodes_words() {
    let dir = scratch("refused");
    let path = plain_key(&dir);
    let node = MockNode::upstream();
    node.state.lock().unwrap().reject_sends = Some("nonce 3 is already pending".into());
    let mut h = harness(&node, &path);
    click(&mut h, "Open");
    click(&mut h, "Send");
    fill(&mut h, "Recipient address", RECIPIENT);
    fill(&mut h, "Amount (PLNE)", "1");
    click(&mut h, "Review");
    click(&mut h, "Sign and send");
    h.get_by_label_contains("Not sent: Transaction rejected: nonce 3 is already pending");
    assert!(
        read_sent(&sent_log_for(&path)).is_empty(),
        "only what the node took is logged"
    );
}

#[test]
fn with_an_upstream_node_history_falls_back_to_the_wallets_own_sends() {
    let dir = scratch("upstream");
    let path = plain_key(&dir);
    let node = MockNode::upstream();
    let mut h = harness(&node, &path);
    click(&mut h, "Open");
    click(&mut h, "Send");
    fill(&mut h, "Recipient address", RECIPIENT);
    fill(&mut h, "Amount (PLNE)", "2");
    click(&mut h, "Review");
    click(&mut h, "Sign and send");

    click(&mut h, "History");
    h.get_by_label_contains("This node keeps no address history");
    h.get_by_label("pending");
    h.get_by_label("-2.00001");

    // The node confirms it: out of the mempool, nonce past it.
    {
        let mut s = node.state.lock().unwrap();
        s.pending.clear();
        s.nonce = 4;
    }
    click(&mut h, "Home");
    click(&mut h, "Settings");
    click(&mut h, "Save and reconnect");
    click(&mut h, "History");
    h.get_by_label("confirmed");
}

#[test]
fn with_an_indexing_node_history_lists_incoming_and_pages() {
    let dir = scratch("history");
    let path = plain_key(&dir);
    let mut entries = vec![entry("aa", 1_200, "in", "transfer", 3_000_000, 1)];
    for h in (1..=60).rev() {
        entries.push(entry(&format!("{h:064x}"), h, "in", "coinbase", 200_000, 0));
    }
    let node = MockNode::with_history(entries);
    // Tall enough that no row is scrolled out of the accessibility tree.
    let mut h = harness_sized(&node, &path, 3_000.0);
    click(&mut h, "Open");
    click(&mut h, "History");
    h.get_by_label("+3.0");
    assert_eq!(
        h.query_all_by_label("+0.2").count(),
        49,
        "the first page holds 50 entries"
    );
    click(&mut h, "Load more");
    assert_eq!(h.query_all_by_label("+0.2").count(), 60);
    assert!(h.query_by_label("Load more").is_none());
}

#[test]
fn a_protected_copy_opens_with_the_new_passphrase() {
    let dir = scratch("protect");
    let path = plain_key(&dir);
    let address = api::inspect(&path).unwrap().address;
    let mut h = harness(&MockNode::upstream(), &path);
    click(&mut h, "Open");
    click(&mut h, "Settings");
    fill(&mut h, "New passphrase", "the protected copy passphrase");
    fill(
        &mut h,
        "Repeat new passphrase",
        "the protected copy passphrase",
    );
    click(&mut h, "Write protected copy");
    h.get_by_label_contains("It opens with the new passphrase");

    let copy = dir.join("plain-protected.plnekey");
    let summary = api::inspect(&copy).unwrap();
    assert_eq!(summary.kdf, "argon2id-v1");
    assert_eq!(summary.address, address);
    let key = api::open(
        &copy,
        Some(&SecretBytes::from_vec(
            b"the protected copy passphrase".to_vec(),
        )),
    );
    assert!(key.is_ok());
    assert!(
        path.exists(),
        "the original stays until its owner removes it"
    );
}

#[test]
fn a_node_that_is_down_is_said_so_and_the_key_still_opens() {
    plaine_wallet_gui::kdf::install();
    let dir = scratch("down");
    let path = plain_key(&dir);
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string();
    let connect: Connect = Arc::new(move |_s: &Settings| {
        Box::new(plaine_wallet_gui::rpc::HttpNode::new(closed.clone(), None)) as Box<dyn Transport>
    });
    let settings = Settings {
        key_file: path.display().to_string(),
        ..Settings::default()
    };
    let mut h = Harness::builder().with_size([900.0, 900.0]).build_ui_state(
        |ui, app: &mut WalletApp| app.show(ui),
        WalletApp::for_tests(settings, connect),
    );
    h.run();
    click(&mut h, "Open");
    assert!(h.state().is_open());
    h.get_by_label_contains("Node: cannot reach the node");
    click(&mut h, "Send");
    fill(&mut h, "Recipient address", RECIPIENT);
    fill(&mut h, "Amount (PLNE)", "1");
    click(&mut h, "Review");
    h.get_by_label_contains("waiting for the node");
}

#[test]
fn the_mining_tab_says_when_the_miner_cannot_start() {
    plaine_wallet_gui::kdf::install();
    let dir = scratch("mining");
    let path = plain_key(&dir);
    let node = MockNode::upstream();
    let connect: Connect =
        Arc::new(move |_s: &Settings| Box::new(node.clone()) as Box<dyn Transport>);
    let settings = Settings {
        key_file: path.display().to_string(),
        miner: dir.join("no-such-miner.exe").display().to_string(),
        ..Settings::default()
    };
    let mut h = Harness::builder().with_size([900.0, 900.0]).build_ui_state(
        |ui, app: &mut WalletApp| app.show(ui),
        WalletApp::for_tests(settings, connect),
    );
    h.run();
    click(&mut h, "Open");
    click(&mut h, "Mining");
    h.get_by_label_contains("paying to this wallet's address");
    assert!(
        h.query_all_by_value("127.0.0.1:9258").count() >= 1,
        "the stratum server defaults to the node's host"
    );
    click(&mut h, "Start mining");
    h.get_by_label_contains("cannot start");
    h.get_by_label("Start mining");
}
