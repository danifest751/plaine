use crate::error::{Result, WalletError};
use crate::secret::{Secret32, SecretBytes};

pub const TAG_KDF: &[u8] = b"PLNE-wallet-kdf-v1";

pub const TAG_ENC: &[u8] = b"PLNE-wallet-enc-v1";

pub const TAG_MAC: &[u8] = b"PLNE-wallet-mac-v1";

pub const TAG_SALT: &[u8] = b"PLNE-wallet-salt-v1";

pub const TAG_NONCE: &[u8] = b"PLNE-wallet-nonce-v1";

pub const KDF_BLAKE3_ITER_V1: &str = "blake3-iter-v1";

pub const KDF_NONE: &str = "none";

pub const CIPHER_BLAKE3_CTR_V1: &str = "blake3-ctr-v1";

pub const CIPHER_NONE: &str = "none";

// Work factor for the passphrase KDF. blake3-iter-v1 is not memory-hard:
// iterations raise the cost per guess linearly and do nothing against a GPU.
pub const DEFAULT_ITERS: u64 = 1_200_000;

pub const WARN_BELOW_ITERS: u64 = 200_000;

// ceiling against a mistyped digit; a runaway count looks just like a hang
pub const MAX_ITERS: u64 = 1_000_000_000;

pub const NOTICE_ABOVE_ITERS: u64 = 4 * DEFAULT_ITERS;

fn null_key() -> Secret32 {
    Secret32::from_bytes([0u8; 32])
}

pub fn derive_key(passphrase: &SecretBytes, salt: &[u8; 32], iters: u64) -> Secret32 {
    let mut h = plaine_consensus::blake3::Hasher::new();
    h.update(TAG_KDF);
    h.update(salt);
    h.update(passphrase.expose());
    let mut k = h.finalize();
    // sequential chain: every round folds in the salt and the counter, which
    // is what keeps the work off a parallel machine
    for i in 0..iters {
        let mut h = plaine_consensus::blake3::Hasher::new();
        h.update(TAG_KDF);
        h.update(salt);
        h.update(&i.to_le_bytes());
        h.update(&k);
        k = h.finalize();
    }
    Secret32::from_bytes(k)
}

pub fn effective_key(passphrase: Option<&SecretBytes>, salt: &[u8; 32], iters: u64) -> Secret32 {
    match passphrase {
        Some(p) => derive_key(p, salt, iters),
        None => null_key(),
    }
}

/// argon2id with fixed memory and lanes; the key file's `kdf_iters` holds the
/// number of passes. Memory-hard: each guess costs 64 MiB, which is what a GPU or
/// an ASIC does not have per core.
pub const KDF_ARGON2ID_V1: &str = "argon2id-v1";

pub const ARGON2_MEMORY_KIB: u32 = 64 * 1024;

pub const ARGON2_LANES: u32 = 1;

pub const ARGON2_DEFAULT_PASSES: u64 = 3;

// A pass over 64 MiB takes a fraction of a second; past this a typo, not a choice.
pub const ARGON2_MAX_PASSES: u64 = 64;

/// The passphrase KDF a key file is sealed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kdf {
    /// `blake3-iter-v1`: iterated BLAKE3, not memory-hard. What files before
    /// argon2id use, and what upstream's wallet reads.
    Blake3Iter { iters: u64 },
    /// `argon2id-v1`: 64 MiB, one lane, `passes` passes.
    Argon2id { passes: u64 },
}

impl Kdf {
    /// What new key files use.
    pub const RECOMMENDED: Kdf = Kdf::Argon2id { passes: ARGON2_DEFAULT_PASSES };

    pub fn name(&self) -> &'static str {
        match self {
            Kdf::Blake3Iter { .. } => KDF_BLAKE3_ITER_V1,
            Kdf::Argon2id { .. } => KDF_ARGON2ID_V1,
        }
    }

    /// The value stored in the key file's `kdf_iters` line.
    pub fn work(&self) -> u64 {
        match self {
            Kdf::Blake3Iter { iters } => *iters,
            Kdf::Argon2id { passes } => *passes,
        }
    }

    /// Reads a key file's `kdf` and `kdf_iters` back. `None` for `none` or a name
    /// this build does not know.
    pub fn from_file(name: &str, work: u64) -> Option<Kdf> {
        match name {
            KDF_BLAKE3_ITER_V1 => Some(Kdf::Blake3Iter { iters: work }),
            KDF_ARGON2ID_V1 => Some(Kdf::Argon2id { passes: work }),
            _ => None,
        }
    }

    pub fn is_memory_hard(&self) -> bool {
        matches!(self, Kdf::Argon2id { .. })
    }

    /// Refuses a work factor that is out of range for this KDF.
    pub fn check(&self) -> Result<()> {
        match *self {
            Kdf::Blake3Iter { iters } if iters > MAX_ITERS => Err(WalletError::usage(format!(
                "--kdf-iters {iters} is above the ceiling of {MAX_ITERS}; at roughly one second \
                 per {DEFAULT_ITERS} iterations, opening the file would take longer than anyone \
                 will wait and buys no real strength"
            ))),
            Kdf::Argon2id { passes } if passes == 0 || passes > ARGON2_MAX_PASSES => {
                Err(WalletError::usage(format!(
                    "argon2id passes must be 1..={ARGON2_MAX_PASSES}, got {passes}; each pass \
                     walks all 64 MiB, so {ARGON2_DEFAULT_PASSES} is already a deliberate cost"
                )))
            }
            _ => Ok(()),
        }
    }

    /// Derives the file key from `passphrase` and the file's salt.
    pub fn derive(&self, passphrase: &SecretBytes, salt: &[u8; 32]) -> Result<Secret32> {
        match *self {
            Kdf::Blake3Iter { iters } => Ok(derive_key(passphrase, salt, iters)),
            Kdf::Argon2id { passes } => {
                argon2id(passphrase, salt, passes, ARGON2_MEMORY_KIB, ARGON2_LANES)
            }
        }
    }
}

/// An argon2id implementation: version 0x13, 32 bytes of output.
///
/// This crate knows the `argon2id-v1` format but does not compute argon2id itself:
/// its dependency policy (tests/end_to_end.rs) admits plaine-consensus,
/// ed25519-dalek and getrandom, and no KDF library. A program that links one
/// installs it once at start-up with [`install_argon2id`]; the desktop wallet in
/// `wallet-gui/` does, and so does its `plaine-wallet-cli`.
pub type Argon2idFn = fn(
    passphrase: &[u8],
    salt: &[u8],
    passes: u32,
    memory_kib: u32,
    lanes: u32,
) -> core::result::Result<[u8; 32], String>;

static ARGON2ID: std::sync::OnceLock<Argon2idFn> = std::sync::OnceLock::new();

/// Installs the argon2id implementation for this process. The first one wins;
/// returns whether this call installed it.
pub fn install_argon2id(f: Argon2idFn) -> bool {
    ARGON2ID.set(f).is_ok()
}

/// Whether this process can seal and open `argon2id-v1` key files.
pub fn argon2id_available() -> bool {
    ARGON2ID.get().is_some()
}

/// argon2id of `passphrase` and `salt` into a 32-byte key, through the installed
/// implementation.
pub fn argon2id(
    passphrase: &SecretBytes,
    salt: &[u8],
    passes: u64,
    memory_kib: u32,
    lanes: u32,
) -> Result<Secret32> {
    let Some(f) = ARGON2ID.get() else {
        return Err(WalletError::format(format!(
            "this program cannot compute {KDF_ARGON2ID_V1}, so it cannot seal or open a key \
             file that uses it. The desktop wallet and plaine-wallet-cli, both built from \
             wallet-gui/, can."
        )));
    };
    let passes = u32::try_from(passes)
        .map_err(|_| WalletError::usage(format!("argon2id passes {passes} is out of range")))?;
    let mut out = f(passphrase.expose(), salt, passes, memory_kib, lanes)
        .map_err(|e| WalletError::crypto(format!("argon2id failed: {e}")))?;
    let key = Secret32::from_bytes(out);
    out.fill(0);
    Ok(key)
}

pub fn keystream_block(key: &Secret32, nonce: &[u8; 16], counter: u64) -> [u8; 32] {
    let mut h = plaine_consensus::blake3::Hasher::new();
    h.update(TAG_ENC);
    h.update(key.expose());
    h.update(nonce);
    h.update(&counter.to_le_bytes());
    h.finalize()
}

pub fn xor_seed(key: &Secret32, nonce: &[u8; 16], input: &[u8; 32]) -> [u8; 32] {
    let ks = keystream_block(key, nonce, 0);
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = input[i] ^ ks[i];
    }
    out
}

pub fn mac(key: &Secret32, message: &[u8]) -> [u8; 32] {
    let mut h = plaine_consensus::blake3::Hasher::new();
    h.update(TAG_MAC);
    h.update(key.expose());
    h.update(message);
    h.finalize()
}

pub fn derive_salt(seed: &Secret32, created: u64) -> [u8; 32] {
    let mut h = plaine_consensus::blake3::Hasher::new();
    h.update(TAG_SALT);
    h.update(seed.expose());
    h.update(&created.to_le_bytes());
    h.finalize()
}

pub fn derive_nonce(seed: &Secret32, created: u64) -> [u8; 16] {
    let mut h = plaine_consensus::blake3::Hasher::new();
    h.update(TAG_NONCE);
    h.update(seed.expose());
    h.update(&created.to_le_bytes());
    let full = h.finalize();
    let mut n = [0u8; 16];
    n.copy_from_slice(&full[..16]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::ct_eq;

    fn pass(s: &str) -> SecretBytes {
        SecretBytes::from_vec(s.as_bytes().to_vec())
    }

    #[test]
    fn derivation_is_deterministic_and_salt_dependent() {
        let salt_a = [0x01u8; 32];
        let salt_b = [0x02u8; 32];
        let k1 = derive_key(&pass("correct horse"), &salt_a, 64);
        let k2 = derive_key(&pass("correct horse"), &salt_a, 64);
        let k3 = derive_key(&pass("correct horse"), &salt_b, 64);
        let k4 = derive_key(&pass("correct horsf"), &salt_a, 64);
        assert_eq!(k1.expose(), k2.expose());
        assert_ne!(k1.expose(), k3.expose(), "the salt must enter the key");
        assert_ne!(k1.expose(), k4.expose(), "the passphrase must enter the key");
    }

    #[test]
    fn iteration_count_changes_the_key() {
        let salt = [0x7Au8; 32];
        let a = derive_key(&pass("x"), &salt, 10);
        let b = derive_key(&pass("x"), &salt, 11);
        assert_ne!(a.expose(), b.expose(), "iters must change the key");
    }

    fn reference_chain(passphrase: &[u8], salt: &[u8; 32], iters: u64) -> [u8; 32] {
        let mut b = Vec::new();
        b.extend_from_slice(TAG_KDF);
        b.extend_from_slice(salt);
        b.extend_from_slice(passphrase);
        let mut k = plaine_consensus::blake3::hash(&b);
        for i in 0..iters {
            let mut b = Vec::new();
            b.extend_from_slice(TAG_KDF);
            b.extend_from_slice(salt);
            b.extend_from_slice(&i.to_le_bytes());
            b.extend_from_slice(&k);
            k = plaine_consensus::blake3::hash(&b);
        }
        k
    }

    #[test]
    fn chain_is_n_salted_hashes() {
        let salt = [0x3Cu8; 32];
        let p = pass("a generated passphrase, not a chosen one");
        for n in [0u64, 1, 2, 3, 17, 64, 255, 256, 1000, 1001, 4096] {
            assert_eq!(
                derive_key(&p, &salt, n).expose(),
                &reference_chain(p.expose(), &salt, n),
                "chain mismatch at {n} iterations"
            );
        }

        let mut seen = std::collections::HashSet::new();
        for n in [0u64, 1, 2, 3, 17, 64, 255, 256, 1000, 1001, 4096] {
            assert!(
                seen.insert(*derive_key(&p, &salt, n).expose()),
                "two different iteration counts produced the same key at {n}"
            );
        }
    }

    #[test]
    fn derive_key_matches_pinned_vector() {
        let salt = [0x11u8; 32];
        let k = derive_key(&pass("plaine kdf known answer"), &salt, WARN_BELOW_ITERS);
        assert_eq!(
            plaine_consensus::hex::encode(k.expose()),
            "2f21c78f1993f4a88506ca5b590d50c579ec85dd319a43113bcaeed044bf3faf",
            "the KDF at {WARN_BELOW_ITERS} iterations no longer matches its frozen vector"
        );
    }

    #[test]
    fn without_an_implementation_argon2id_is_refused_by_name() {
        // Nothing in this crate's unit tests installs one.
        let e = argon2id(&pass("x"), &[0u8; 32], 3, 64 * 1024, 1).unwrap_err();
        assert!(e.to_string().contains("argon2id-v1"), "{e}");
        assert!(e.to_string().contains("wallet-gui"), "{e}");
    }

    #[test]
    fn kdf_names_and_bounds() {
        assert_eq!(Kdf::from_file(KDF_ARGON2ID_V1, 3), Some(Kdf::Argon2id { passes: 3 }));
        assert_eq!(Kdf::from_file(KDF_BLAKE3_ITER_V1, 9), Some(Kdf::Blake3Iter { iters: 9 }));
        assert_eq!(Kdf::from_file(KDF_NONE, 0), None);
        assert!(Kdf::RECOMMENDED.is_memory_hard());
        assert!(Kdf::Argon2id { passes: 0 }.check().is_err());
        assert!(Kdf::Argon2id { passes: ARGON2_MAX_PASSES + 1 }.check().is_err());
        assert!(Kdf::Blake3Iter { iters: MAX_ITERS + 1 }.check().is_err());
        assert!(Kdf::RECOMMENDED.check().is_ok());
    }

    #[test]
    fn xor_seed_is_its_own_inverse() {
        let key = Secret32::from_bytes([0x5Au8; 32]);
        let nonce = [0x33u8; 16];
        let seed = plaine_consensus::blake3::hash(b"kdf xor test");
        let ct = xor_seed(&key, &nonce, &seed);
        assert_ne!(ct, seed, "ciphertext must not equal plaintext");
        assert_eq!(xor_seed(&key, &nonce, &ct), seed);
    }

    #[test]
    fn nonce_separates_the_keystream() {
        let key = Secret32::from_bytes([0x11u8; 32]);
        assert_ne!(
            keystream_block(&key, &[0u8; 16], 0),
            keystream_block(&key, &[1u8; 16], 0)
        );
        assert_ne!(
            keystream_block(&key, &[0u8; 16], 0),
            keystream_block(&key, &[0u8; 16], 1)
        );
    }

    #[test]
    fn domains_are_separated() {
        let key = Secret32::from_bytes([0x99u8; 32]);
        let m = mac(&key, b"");
        let ks = keystream_block(&key, &[0u8; 16], 0);
        assert_ne!(m, ks);
    }

    #[test]
    fn mac_is_sensitive_to_every_message_byte() {
        let key = Secret32::from_bytes([0x42u8; 32]);
        let base = mac(&key, b"kdf: blake3-iter-v1\nkdf_iters: 1200000\n");
        let down = mac(&key, b"kdf: none\nkdf_iters: 0\n");
        assert!(!ct_eq(&base, &down), "a downgrade must move the MAC");
    }

    #[test]
    fn salt_and_nonce_are_unique_per_seed_and_time() {
        let s1 = Secret32::from_bytes(plaine_consensus::blake3::hash(b"salt seed 1"));
        let s2 = Secret32::from_bytes(plaine_consensus::blake3::hash(b"salt seed 2"));
        assert_ne!(derive_salt(&s1, 100), derive_salt(&s2, 100));
        assert_ne!(derive_salt(&s1, 100), derive_salt(&s1, 101));
        assert_ne!(derive_nonce(&s1, 100), derive_nonce(&s2, 100));

        assert_ne!(&derive_salt(&s1, 100), s1.expose());
    }
}
