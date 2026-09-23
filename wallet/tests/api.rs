// The library face of the wallet, as a GUI uses it: no terminal, no argv, every
// result and every piece of advice returned as data.

use plaine_consensus::codec::TransferTx;
use plaine_consensus::constants::{Network, FEE_FLOOR_MILE};
use plaine_wallet::api::{self, Kdf, Notice};
use plaine_wallet::error::WalletError;
use plaine_wallet::keyfile::Role;
use plaine_wallet::secret::SecretBytes;
use std::path::PathBuf;

// Low enough to keep the suite fast; the work factor is not what is tested here.
const ITERS: u64 = 1_000;
const FAST: Kdf = Kdf::Blake3Iter { iters: ITERS };

fn scratch(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "plaine-wallet-api-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn pass(s: &str) -> SecretBytes {
    SecretBytes::from_vec(s.as_bytes().to_vec())
}

#[test]
fn a_generated_key_opens_with_its_passphrase_and_nothing_else() {
    let dir = scratch("generated");
    let path = dir.join("k.plnekey");
    let created = api::create_key(&path, Role::Spend, None, Some(&pass("correct horse")), FAST)
        .expect("create");
    assert!(created.generated);
    assert!(created.summary.encrypted);
    assert!(created.summary.address.starts_with("plne1"));

    let key = api::open(&path, Some(&pass("correct horse"))).expect("open");
    assert_eq!(key.address(), created.summary.address);
    assert_eq!(key.summary(), api::inspect(&path).expect("inspect"));

    let wrong = api::open(&path, Some(&pass("wrong horse"))).unwrap_err();
    assert!(matches!(wrong, WalletError::Crypto(_)), "{wrong}");
    assert!(api::open(&path, None).is_err(), "an encrypted file needs its passphrase");

    let again = api::create_key(&path, Role::Spend, None, None, FAST).unwrap_err();
    assert!(matches!(again, WalletError::Refused(_)), "a key file is never overwritten: {again}");
}

#[test]
fn the_backup_string_restores_the_same_address() {
    let dir = scratch("backup");
    let original = dir.join("a.plnekey");
    let created = api::create_key(&original, Role::Spend, None, None, FAST).expect("create");
    assert_eq!(created.notices, [Notice::Unencrypted], "no passphrase is said out loud");

    let backup = api::open(&original, None).expect("open").backup_string();
    assert_eq!(backup.len(), 68);

    let (seed, notice) = api::decode_seed_text(&format!("  {backup}\n"), true).expect("decode");
    assert_eq!(notice, None, "a checksummed string needs no warning");
    let restored = dir.join("b.plnekey");
    let r = api::create_key(&restored, Role::Spend, Some(seed), Some(&pass("p")), FAST)
        .expect("restore");
    assert!(!r.generated);
    assert_eq!(r.summary.address, created.summary.address);
}

#[test]
fn seed_text_is_checked_the_way_the_cli_checks_it() {
    let dir = scratch("seedtext");
    let path = dir.join("k.plnekey");
    api::create_key(&path, Role::Spend, None, None, FAST).expect("create");
    let backup = api::open(&path, None).expect("open").backup_string();

    let bare = &backup[..64];
    assert!(
        matches!(api::decode_seed_text(bare, true), Err(WalletError::Refused(_))),
        "restoring refuses 64 bare digits: nothing would catch a slip"
    );
    let (_, notice) = api::decode_seed_text(bare, false).expect("raw entropy for a new key");
    assert_eq!(notice, Some(Notice::UncheckedSeed));

    let mut slipped = backup.clone().into_bytes();
    slipped[10] = if slipped[10] == b'0' { b'1' } else { b'0' };
    let slipped = String::from_utf8(slipped).unwrap();
    assert!(
        matches!(api::decode_seed_text(&slipped, true), Err(WalletError::Refused(_))),
        "a one-character slip fails the checksum"
    );
}

#[test]
fn a_signed_transfer_verifies_and_round_trips() {
    let dir = scratch("sign");
    let path = dir.join("k.plnekey");
    api::create_key(&path, Role::Spend, None, Some(&pass("p")), FAST).expect("create");
    let key = api::open(&path, Some(&pass("p"))).expect("open");
    let to = "plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5";

    let t = key.sign_transfer(Network::Main, to, 150_000, FEE_FLOOR_MILE, 7).expect("sign");
    assert_eq!((t.amount, t.fee, t.nonce), (150_000, FEE_FLOOR_MILE, 7));
    assert_eq!(t.from, key.address());
    assert_eq!(t.to, to);
    assert_eq!(t.hex, plaine_consensus::hex::encode(&t.raw));

    let decoded = TransferTx::decode(&t.raw).expect("decode");
    assert_eq!(decoded.txid(), t.txid);
    plaine_consensus::crypto::verify_transfer_signature(Network::Main, &decoded)
        .expect("the signature verifies");

    assert!(key.sign_transfer(Network::Main, "plne1typo", 1, FEE_FLOOR_MILE, 0).is_err());
    assert!(
        key.sign_transfer(Network::Main, to, 1, 0, 0).is_err(),
        "a fee below the consensus floor is refused before signing"
    );
}

#[test]
fn rewrap_changes_the_passphrase_and_leaves_the_original() {
    let dir = scratch("rewrap");
    let old = dir.join("old.plnekey");
    let new = dir.join("new.plnekey");
    api::create_key(&old, Role::Spend, None, Some(&pass("old")), FAST).expect("create");
    let key = api::open(&old, Some(&pass("old"))).expect("open");

    let (summary, carried) = key.rewrap(&new, Some(&pass("new")), FAST).expect("rewrap");
    assert_eq!(carried, None, "there was no journal to carry");
    assert_eq!(summary.address, key.address());
    assert!(api::open(&new, Some(&pass("new"))).is_ok());
    assert!(api::open(&new, Some(&pass("old"))).is_err());
    assert!(api::open(&old, Some(&pass("old"))).is_ok(), "the original is untouched");

    assert!(matches!(key.rewrap(&old, None, FAST), Err(WalletError::Refused(_))));
    assert!(matches!(key.rewrap(&new, None, FAST), Err(WalletError::Refused(_))));
}

#[test]
fn creation_advice_matches_the_choice() {
    assert_eq!(api::create_notices(Role::Spend, false, FAST), [Notice::Unencrypted]);
    assert_eq!(
        api::create_notices(
            Role::Spend,
            true,
            Kdf::Blake3Iter { iters: plaine_wallet::kdf::DEFAULT_ITERS }
        ),
        [Notice::KdfNotMemoryHard, Notice::KeepPassphraseApart]
    );
    let few = api::create_notices(Role::Author, true, FAST);
    assert_eq!(few.first(), Some(&Notice::FewIterations { iters: ITERS }));
    assert_eq!(few.last(), Some(&Notice::AirGapThisRole { memory_hard: false }));
    for n in few {
        assert!(!n.lines().is_empty() && n.lines().iter().all(|l| !l.contains("  ")), "{n:?}");
    }
    assert!(api::check_iters(plaine_wallet::kdf::MAX_ITERS + 1).is_err());
}

#[test]
fn argon2id_without_an_implementation_is_refused_and_writes_nothing() {
    // Nothing in this test binary installs one, as in the plain plaine-wallet CLI.
    assert!(!plaine_wallet::kdf::argon2id_available());
    let dir = scratch("no-argon2id");
    let path = dir.join("k.plnekey");
    let e = api::create_key(&path, Role::Spend, None, Some(&pass("p")), Kdf::RECOMMENDED)
        .unwrap_err();
    assert!(e.to_string().contains("argon2id-v1"), "{e}");
    assert!(!path.exists(), "a key file that could not be sealed is not left behind");
}

#[test]
fn amounts_and_addresses_as_a_form_takes_them() {
    assert_eq!(api::parse_plne("1.5").unwrap(), 1_500_000);
    assert_eq!(api::parse_plne(" 0.000001 ").unwrap(), 1);
    assert!(api::parse_plne("").is_err());
    assert!(api::parse_plne("1.5plne").is_err(), "the field is already in PLNE");
    assert!(api::parse_plne("0.0000001").is_err(), "finer than a mile");
    assert_eq!(api::parse_amount("1500000mile").unwrap(), 1_500_000);
    assert_eq!(api::format_plne(1_500_000), "1.5");

    assert!(api::check_address("plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp5").is_ok());
    assert!(api::check_address("plne1pjvhejseh7veg36dvuqn239puwu7rfsuf57xp4").is_err());
    assert!(api::check_address("").is_err());
}
