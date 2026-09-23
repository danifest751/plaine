// New key files can be sealed under argon2id; every key file made before keeps
// opening. The fixtures were written by upstream's wallet at 189598d, unmodified
// (see tests/fixtures/README.md): a `kdf: none` file, like a mining key made with
// the README's quick start, and a `blake3-iter-v1` file.
//
// argon2id itself is not in this crate (its dependency policy admits no KDF
// library), so these tests install a stand-in and check the plumbing: format,
// sealing, opening, downgrade protection. wallet-gui/tests/kdf.rs runs the same
// paths with the real argon2id against the reference vectors.

use plaine_consensus::constants::{Network, FEE_FLOOR_MILE};
use plaine_wallet::api::{self, Kdf, Notice};
use plaine_wallet::error::WalletError;
use plaine_wallet::keyfile::{self, Role};
use plaine_wallet::secret::SecretBytes;
use std::path::PathBuf;

const FIXTURE_ADDRESS: &str = "plne1ned7pg9p8wk3jdu69m5g7yt39yc4s6ye8zgdk9";
const FIXTURE_PASSPHRASE: &str = "fixture passphrase";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("plaine-wallet-kdf-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn pass(s: &str) -> SecretBytes {
    SecretBytes::from_vec(s.as_bytes().to_vec())
}

/// Deterministic, parameter-sensitive, and plainly not argon2id.
fn stand_in(
    passphrase: &[u8],
    salt: &[u8],
    passes: u32,
    memory_kib: u32,
    lanes: u32,
) -> Result<[u8; 32], String> {
    let mut b = b"stand-in for argon2id in tests".to_vec();
    for part in [&passes.to_le_bytes()[..], &memory_kib.to_le_bytes(), &lanes.to_le_bytes(), salt, passphrase] {
        b.extend_from_slice(&(part.len() as u64).to_le_bytes());
        b.extend_from_slice(part);
    }
    Ok(plaine_consensus::blake3::hash(&b))
}

fn with_argon2id() {
    plaine_wallet::kdf::install_argon2id(stand_in);
}

#[test]
fn an_unencrypted_upstream_key_still_opens_and_signs() {
    let key = api::open(&fixture("upstream-none.plnekey"), None).expect("open");
    assert_eq!(key.address(), FIXTURE_ADDRESS);
    assert_eq!(key.summary().kdf, "none");
    let t = key
        .sign_transfer(Network::Main, FIXTURE_ADDRESS, 1, FEE_FLOOR_MILE, 0)
        .expect("a kdf: none key signs as before");
    assert_eq!(t.from, FIXTURE_ADDRESS);
}

#[test]
fn a_blake3_iter_upstream_key_still_opens() {
    let path = fixture("upstream-blake3.plnekey");
    let key = api::open(&path, Some(&pass(FIXTURE_PASSPHRASE))).expect("open");
    assert_eq!(key.address(), FIXTURE_ADDRESS);
    assert_eq!(key.summary().kdf, "blake3-iter-v1");
    assert!(api::open(&path, Some(&pass("not it"))).is_err());
}

#[test]
fn an_old_key_moves_to_argon2id_and_the_original_stays() {
    with_argon2id();
    let dir = scratch("migrate");
    let old = fixture("upstream-none.plnekey");
    let before = std::fs::read(&old).expect("read fixture");
    let new = dir.join("migrated.plnekey");

    let key = api::open(&old, None).expect("open");
    let (summary, _) = key.rewrap(&new, Some(&pass("a new passphrase")), Kdf::RECOMMENDED)
        .expect("rewrap");
    assert_eq!(summary.kdf, "argon2id-v1");
    assert_eq!(summary.kdf_iters, 3, "kdf_iters holds the argon2id passes");
    assert_eq!(summary.address, FIXTURE_ADDRESS, "same seed, same address");

    let reopened = api::open(&new, Some(&pass("a new passphrase"))).expect("open migrated");
    assert_eq!(reopened.address(), FIXTURE_ADDRESS);
    assert!(api::open(&new, Some(&pass("a wrong one"))).is_err());
    assert!(api::open(&new, None).is_err(), "an argon2id file needs its passphrase");
    assert_eq!(std::fs::read(&old).expect("reread"), before, "the original is untouched");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_new_key_is_sealed_under_argon2id_without_the_blake3_warning() {
    with_argon2id();
    let dir = scratch("new");
    let path = dir.join("k.plnekey");
    let created =
        api::create_key(&path, Role::Spend, None, Some(&pass("p")), Kdf::RECOMMENDED).expect("create");
    assert_eq!(created.summary.kdf, "argon2id-v1");
    assert!(!created.notices.contains(&Notice::KdfNotMemoryHard), "{:?}", created.notices);
    assert!(created.notices.contains(&Notice::KeepPassphraseApart));

    let author = api::create_notices(Role::Author, true, Kdf::RECOMMENDED);
    assert_eq!(author.last(), Some(&Notice::AirGapThisRole { memory_hard: true }));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_argon2id_file_cannot_be_quietly_downgraded() {
    with_argon2id();
    let dir = scratch("downgrade");
    let path = dir.join("k.plnekey");
    api::create_key(&path, Role::Spend, None, Some(&pass("p")), Kdf::RECOMMENDED).expect("create");
    let text = std::fs::read_to_string(&path).expect("read");

    // Relabelled as the weaker KDF: the MAC covers the kdf line.
    let relabelled = text.replace("kdf: argon2id-v1", "kdf: blake3-iter-v1");
    std::fs::write(&path, &relabelled).expect("write");
    assert!(matches!(api::open(&path, Some(&pass("p"))), Err(WalletError::Crypto(_))));

    // Fewer passes: the MAC covers kdf_iters too.
    let cheaper = text.replace("kdf_iters: 3", "kdf_iters: 1");
    std::fs::write(&path, &cheaper).expect("write");
    assert!(matches!(api::open(&path, Some(&pass("p"))), Err(WalletError::Crypto(_))));

    // Out of range for argon2id is refused on reading, before any work is done.
    let absurd = text.replace("kdf_iters: 3", "kdf_iters: 1200000");
    assert!(matches!(keyfile::KeyFile::parse(&absurd), Err(WalletError::Format(_))));
    let _ = std::fs::remove_dir_all(dir);
}
