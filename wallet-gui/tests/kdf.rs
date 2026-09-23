// The real argon2id behind plaine-wallet's `argon2id-v1` key files: checked against
// the reference implementation, then driven through the wallet's own code and the
// plaine-wallet-cli binary. Old key files, written by upstream's wallet, keep
// opening next to it.

use plaine_wallet::api::{self, Kdf};
use plaine_wallet::keyfile::Role;
use plaine_wallet::secret::SecretBytes;
use std::path::PathBuf;
use std::process::Command;

const FIXTURE_ADDRESS: &str = "plne1ned7pg9p8wk3jdu69m5g7yt39yc4s6ye8zgdk9";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("wallet")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("plaine-gui-kdf-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn pass(s: &str) -> SecretBytes {
    SecretBytes::from_vec(s.as_bytes().to_vec())
}

#[test]
fn argon2id_matches_the_reference_implementation() {
    // phc-winner-argon2's test.c: argon2id, v=0x13, t=2, m=2^16 KiB (the 64 MiB the
    // key files use), p=1, password "password", salt "somesalt".
    let k = plaine_wallet_gui::kdf::argon2id(b"password", b"somesalt", 2, 1 << 16, 1)
        .expect("argon2id");
    assert_eq!(
        plaine_consensus::hex::encode(&k),
        "09316115d5cf24ed5a15a31a3ba326e5cf32edc24702987c02b6566f61913cf7"
    );
}

#[test]
fn the_file_kdf_is_argon2id_at_64_mib_and_one_lane() {
    plaine_wallet_gui::kdf::install();
    let salt = [0x11u8; 32];
    let p = pass("plaine kdf known answer");
    let via_wallet = Kdf::Argon2id { passes: 3 }
        .derive(&p, &salt)
        .expect("derive");
    let direct = plaine_wallet_gui::kdf::argon2id(p.expose(), &salt, 3, 64 * 1024, 1).unwrap();
    assert_eq!(via_wallet.expose(), &direct);
}

#[test]
fn an_upstream_key_moves_to_argon2id_with_the_same_address() {
    plaine_wallet_gui::kdf::install();
    let dir = scratch("migrate");
    let new = dir.join("k.plnekey");
    let key = api::open(&fixture("upstream-none.plnekey"), None).expect("open old file");
    key.rewrap(&new, Some(&pass("new passphrase")), Kdf::RECOMMENDED)
        .expect("rewrap");

    let reopened = api::open(&new, Some(&pass("new passphrase"))).expect("open argon2id file");
    assert_eq!(reopened.address(), FIXTURE_ADDRESS);
    assert_eq!(reopened.summary().kdf, "argon2id-v1");
    assert!(api::open(&new, Some(&pass("not it"))).is_err());

    let blake3 = api::open(
        &fixture("upstream-blake3.plnekey"),
        Some(&pass("fixture passphrase")),
    )
    .expect("the blake3-iter-v1 file opens alongside");
    assert_eq!(blake3.address(), FIXTURE_ADDRESS);

    let fresh = dir.join("fresh.plnekey");
    let created = api::create_key(
        &fresh,
        Role::Spend,
        None,
        Some(&pass("p")),
        Kdf::RECOMMENDED,
    )
    .expect("create");
    assert_eq!(created.summary.kdf, "argon2id-v1");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn plaine_wallet_cli_seals_and_opens_argon2id() {
    let dir = scratch("cli");
    let key = dir.join("k.plnekey");
    let pass_file = dir.join("p.txt");
    std::fs::write(&pass_file, "cli test passphrase").unwrap();
    let cli = env!("CARGO_BIN_EXE_plaine-wallet-cli");

    let made = Command::new(cli)
        .args(["new", "--role", "spend", "--kdf", "argon2id", "--out"])
        .arg(&key)
        .arg("--passphrase-file")
        .arg(&pass_file)
        .output()
        .expect("run plaine-wallet-cli");
    let text = String::from_utf8_lossy(&made.stdout);
    assert!(
        made.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&made.stderr)
    );
    assert!(text.contains("kdf        argon2id-v1 (iters 3)"), "{text}");

    let verified = Command::new(cli)
        .args(["verify", "--in"])
        .arg(&key)
        .arg("--passphrase-file")
        .arg(&pass_file)
        .output()
        .expect("run plaine-wallet-cli");
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );

    let old = Command::new(cli)
        .args(["verify", "--no-passphrase", "--in"])
        .arg(fixture("upstream-none.plnekey"))
        .output()
        .expect("run plaine-wallet-cli");
    assert!(
        String::from_utf8_lossy(&old.stdout).contains(FIXTURE_ADDRESS),
        "a kdf: none key still opens: {}",
        String::from_utf8_lossy(&old.stderr)
    );
    let _ = std::fs::remove_dir_all(dir);
}
