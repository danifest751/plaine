// The desktop wallet end to end, against a real plaine-noded and plaine-miner on a
// fresh chain: two keys made in the GUI, A mined past coinbase maturity, A sends to
// B through the send screen, and both sides see it; the balances add up to what
// emission_audit says was issued.
//
// It mines past maturity, so it takes minutes: ignored by default, run by
// `scripts/check.sh --e2e`. It needs the node and the miner built in release
// (`cargo build --release` and `--manifest-path miner/Cargo.toml`), which the check
// script does first.

use eframe::egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use plaine_wallet_gui::app::{Connect, WalletApp};
use plaine_wallet_gui::model::Settings;
use plaine_wallet_gui::rpc::{self, HttpNode, Transport};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const P2P: u16 = 20_601;
const RPC: u16 = 20_602;
const STRATUM: u16 = 20_603;
const MATURITY: u64 = 60;

/// Both tests use the same ports, so they take turns even under a parallel runner.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn exe(rel: &str) -> PathBuf {
    let mut p = root().join(rel);
    if cfg!(windows) {
        p.set_extension("exe");
    }
    assert!(
        p.exists(),
        "{} is missing; build it in release first",
        p.display()
    );
    p
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_node(dir: &Path) -> Killed {
    let data = dir.join("data");
    let cfg = dir.join("noded.toml");
    std::fs::write(
        &cfg,
        format!(
            "[node]\nnetwork = \"main\"\ndata_dir = {:?}\nprune = false\naddrindex = true\n\n\
             [p2p]\nlisten = \"127.0.0.1:{P2P}\"\nuse_embedded_seeds = false\n\n\
             [rpc]\nlisten = \"127.0.0.1:{RPC}\"\n\n[stratum]\nlisten = \"127.0.0.1:{STRATUM}\"\n",
            data.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let log = std::fs::File::create(dir.join("node.log")).unwrap();
    let child = Command::new(exe("target/release/plaine-noded"))
        .arg("--config")
        .arg(&cfg)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("start plaine-noded");
    let node = Killed(child);
    wait("the node's RPC", Duration::from_secs(60), || {
        rpc::chain_info(&http()).is_ok()
    });
    node
}

fn http() -> HttpNode {
    HttpNode::new(format!("127.0.0.1:{RPC}"), None)
}

fn height() -> u64 {
    rpc::chain_info(&http()).map(|c| c.height).unwrap_or(0)
}

fn wait(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !done() {
        assert!(t0.elapsed() < within, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Mines to `address` until the chain reaches `target`, leaving two cores free.
fn mine_to(address: &str, target: u64, within: Duration) {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(2);
    let child = Command::new(exe("miner/target/release/plaine-miner"))
        .args(["--address", &format!("{address}.e2e"), "--stratum"])
        .arg(format!("127.0.0.1:{STRATUM}"))
        .args([
            "--threads",
            &threads.to_string(),
            "--deadline",
            &within.as_secs().to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start plaine-miner");
    let _miner = Killed(child);
    wait(&format!("mining to height {target}"), within, || {
        height() >= target
    });
}

fn gui(key_file: &Path) -> Harness<'static, WalletApp> {
    plaine_wallet_gui::kdf::install();
    let connect: Connect = Arc::new(|_s: &Settings| Box::new(http()) as Box<dyn Transport>);
    let settings = Settings {
        key_file: key_file.display().to_string(),
        ..Settings::default()
    };
    let mut h = Harness::builder()
        .with_size([1000.0, 2000.0])
        .build_ui_state(
            |ui, app: &mut WalletApp| app.show(ui),
            WalletApp::for_tests(settings, connect),
        );
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

fn click(h: &mut Harness<'static, WalletApp>, label: &str) {
    h.get_by_label(label).click();
    h.run();
}

/// Creates a key through the first-run screen and returns its address.
fn create(h: &mut Harness<'static, WalletApp>, passphrase: &str, key_file: &Path) -> String {
    click(h, "Create a new key");
    fill(h, "Passphrase", passphrase);
    fill(h, "Repeat passphrase", passphrase);
    click(h, "Create");
    assert!(h.state().is_open(), "the key was created and opened");
    click(h, "I have written it down");
    plaine_wallet::api::inspect(key_file).unwrap().address
}

fn balance(address: &str) -> rpc::Account {
    rpc::account(&http(), address).unwrap()
}

#[test]
#[ignore = "mines past coinbase maturity, minutes of real proof of work; scripts/check.sh --e2e runs it"]
fn two_keys_made_in_the_gui_trade_through_the_send_screen() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("plaine-gui-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _node = start_node(&dir);

    let key_a = dir.join("a.plnekey");
    let key_b = dir.join("b.plnekey");
    let mut a = gui(&key_a);
    let addr_a = create(&mut a, "passphrase of key a", &key_a);
    let mut b = gui(&key_b);
    let addr_b = create(&mut b, "passphrase of key b", &key_b);

    mine_to(&addr_a, MATURITY + 2, Duration::from_secs(1_200));

    // The GUI shows the node's own figure.
    click(&mut a, "Refresh");
    let before = balance(&addr_a);
    assert!(
        before.spendable >= 200_000,
        "a matured coinbase: {before:?}"
    );
    let shown = format!("{} PLNE", plaine_wallet::api::format_plne(before.spendable));
    assert!(
        a.query_all_by_label(&shown).count() >= 1,
        "the GUI shows {shown}"
    );

    // A sends 0.1 PLNE to B through the send screen.
    click(&mut a, "Send");
    fill(&mut a, "Recipient address", &addr_b);
    fill(&mut a, "Amount (PLNE)", "0.1");
    click(&mut a, "Review");
    a.get_by_label("Confirm");
    click(&mut a, "Sign and send");
    a.get_by_label_contains("Sent: ");
    let pending = rpc::pending(&http(), &addr_a).unwrap();
    assert_eq!(pending.len(), 1, "the transfer waits in the mempool");

    // One more block (or two) takes it.
    let h = height();
    mine_to(&addr_a, h + 2, Duration::from_secs(300));
    wait("the transfer to confirm", Duration::from_secs(60), || {
        rpc::pending(&http(), &addr_a)
            .map(|p| p.is_empty())
            .unwrap_or(false)
    });

    click(&mut b, "Refresh");
    assert!(
        b.query_all_by_label("0.1 PLNE").count() >= 1,
        "B's balance shows the transfer"
    );
    click(&mut b, "History");
    b.get_by_label("+0.1");
    b.get_by_label("transfer in");

    click(&mut a, "Refresh");
    click(&mut a, "History");
    a.get_by_label("transfer out");

    // Nothing was created or lost: everything issued sits with A and B.
    let issued: u128 = http().call("emission_audit", json!([])).unwrap()["issuedMile"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let held = balance(&addr_a).balance + balance(&addr_b).balance;
    assert_eq!(held, issued, "A and B hold every mile issued on this chain");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "runs a real node and miner; scripts/check.sh --e2e runs it"]
fn the_mining_tab_mines_to_the_wallet_and_shows_accepted_shares() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("plaine-gui-mining-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _node = start_node(&dir);

    let key = dir.join("m.plnekey");
    plaine_wallet::api::create_key(
        &key,
        plaine_wallet::keyfile::Role::Spend,
        None,
        None,
        plaine_wallet::api::Kdf::Blake3Iter { iters: 0 },
    )
    .unwrap();
    let address = plaine_wallet::api::inspect(&key).unwrap().address;

    plaine_wallet_gui::kdf::install();
    let connect: Connect = Arc::new(|_s: &Settings| Box::new(http()) as Box<dyn Transport>);
    let settings = Settings {
        key_file: key.display().to_string(),
        node: format!("127.0.0.1:{RPC}"),
        stratum: format!("127.0.0.1:{STRATUM}"),
        miner: exe("miner/target/release/plaine-miner")
            .display()
            .to_string(),
        rig: "gui".into(),
        ..Settings::default()
    };
    let mut h = Harness::builder()
        .with_size([1000.0, 1200.0])
        .build_ui_state(
            |ui, app: &mut WalletApp| app.show(ui),
            WalletApp::for_tests(settings, connect),
        );
    h.run();
    click(&mut h, "Open");
    click(&mut h, "Mining");
    click(&mut h, "Start mining");
    h.get_by_label("Stop mining");

    // The tab fills in from the miner's JSON lines as shares are accepted.
    let t0 = Instant::now();
    let accepted = loop {
        h.run();
        let shown = h
            .query_all_by_label_contains(" accepted, ")
            .filter_map(|n| n.value())
            .find_map(|t| t.split(' ').next()?.parse::<u64>().ok())
            .unwrap_or(0);
        if shown >= 2 {
            break shown;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(180),
            "no accepted shares shown in 180 s"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(accepted >= 2);

    // The node sees the wallet's worker, paying to the wallet's address. A session
    // can be caught between a reconnect and its login, so this waits for one.
    let worker = format!("{address}.gui");
    let t0 = Instant::now();
    loop {
        let sessions = http().call("stratum_getSessions", json!([])).unwrap();
        let seen = sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["worker"] == worker.as_str() && s["authorized"] == true);
        if seen {
            break;
        }
        if t0.elapsed() > Duration::from_secs(20) {
            h.run();
            let miner_said: Vec<String> = h
                .query_all_by_label_contains("miner: ")
                .filter_map(|n| n.value())
                .collect();
            panic!("the wallet's worker never logged in: {sessions}; {miner_said:?}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    click(&mut h, "Stop mining");
    h.get_by_label("Start mining");
    assert!(
        h.query_by_label("mining").is_none(),
        "the state is no longer mining"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
