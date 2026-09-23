use crate::error::{Result, WalletError};
use crate::kdf;
use crate::sechex;
use crate::secret::{ct_eq, Secret32, SecretBytes};
use crate::sig;
use plaine_consensus::crypto;
use std::path::Path;

pub const MAGIC: &str = "PLNEKEY1";

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Spend,
    Author,
    Checkpoint,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Spend => "spend",
            Role::Author => "author",
            Role::Checkpoint => "checkpoint",
        }
    }

    pub fn parse(s: &str) -> Result<Role> {
        match s {
            "spend" => Ok(Role::Spend),
            "author" => Ok(Role::Author),
            "checkpoint" => Ok(Role::Checkpoint),
            other => Err(WalletError::format(format!(
                "unknown role {other:?}; expected spend, author or checkpoint"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFile {
    pub version: u32,
    pub role: Role,
    pub created: u64,
    pub kdf: String,
    pub kdf_iters: u64,
    pub kdf_salt: [u8; 32],
    pub cipher: String,
    pub nonce: [u8; 16],
    pub pubkey: [u8; 32],
    pub address: String,
    pub ciphertext: [u8; 32],
    pub mac: [u8; 32],
}

const FIELDS: [&str; 12] = [
    "version",
    "role",
    "created",
    "kdf",
    "kdf_iters",
    "kdf_salt",
    "cipher",
    "nonce",
    "pubkey",
    "address",
    "ciphertext",
    "mac",
];

impl KeyFile {
    /// Seals under `blake3-iter-v1`, the KDF upstream's wallet reads.
    pub fn seal(
        role: Role,
        created: u64,
        seed: &Secret32,
        passphrase: Option<&SecretBytes>,
        iters: u64,
    ) -> Result<KeyFile> {
        Self::seal_with(role, created, seed, passphrase, kdf::Kdf::Blake3Iter { iters })
    }

    /// Seals under `kdf` when there is a passphrase; with none, the seed is stored
    /// in the clear and labelled `kdf: none`.
    pub fn seal_with(
        role: Role,
        created: u64,
        seed: &Secret32,
        passphrase: Option<&SecretBytes>,
        with: kdf::Kdf,
    ) -> Result<KeyFile> {
        let pubkey = sig::public_key_of(seed);
        let address = crypto::address_from_pubkey(&pubkey);
        let salt = kdf::derive_salt(seed, created);
        let nonce = kdf::derive_nonce(seed, created);

        let (kdf_name, cipher_name, iters) = match passphrase {
            Some(_) => {
                with.check()?;
                (with.name().to_string(), kdf::CIPHER_BLAKE3_CTR_V1.to_string(), with.work())
            }
            None => (kdf::KDF_NONE.to_string(), kdf::CIPHER_NONE.to_string(), 0),
        };

        let key = match passphrase {
            Some(p) => with.derive(p, &salt)?,
            None => kdf::effective_key(None, &salt, 0),
        };
        let ciphertext = match passphrase {
            Some(_) => kdf::xor_seed(&key, &nonce, seed.expose()),
            None => *seed.expose(),
        };

        let mut kf = KeyFile {
            version: FORMAT_VERSION,
            role,
            created,
            kdf: kdf_name,
            kdf_iters: iters,
            kdf_salt: salt,
            cipher: cipher_name,
            nonce,
            pubkey,
            address,
            ciphertext,
            mac: [0u8; 32],
        };
        kf.mac = kdf::mac(&key, kf.mac_input().as_bytes());
        Ok(kf)
    }

    pub fn open(&self, passphrase: Option<&SecretBytes>) -> Result<Secret32> {
        let uses_kdf = self.kdf != kdf::KDF_NONE;
        if uses_kdf && passphrase.is_none() {
            return Err(WalletError::usage(format!(
                "this key file uses kdf {:?}; supply --passphrase-file or --passphrase-stdin",
                self.kdf
            )));
        }
        if !uses_kdf && passphrase.is_some() {
            return Err(WalletError::usage(
                "this key file has kdf: none and is not encrypted; do not pass a passphrase",
            ));
        }
        let with = kdf::Kdf::from_file(&self.kdf, self.kdf_iters);
        if uses_kdf && with.is_none() {
            return Err(WalletError::format(format!(
                "unsupported kdf {:?}; this build understands {:?}, {:?} and {:?}",
                self.kdf,
                kdf::KDF_ARGON2ID_V1,
                kdf::KDF_BLAKE3_ITER_V1,
                kdf::KDF_NONE
            )));
        }
        if uses_kdf && self.cipher != kdf::CIPHER_BLAKE3_CTR_V1 {
            return Err(WalletError::format(format!(
                "unsupported cipher {:?}",
                self.cipher
            )));
        }

        let key = match (with, passphrase) {
            (Some(w), Some(p)) => w.derive(p, &self.kdf_salt)?,
            _ => kdf::effective_key(None, &self.kdf_salt, 0),
        };
        let expect = kdf::mac(&key, self.mac_input().as_bytes());
        if !ct_eq(&expect, &self.mac) {
            return Err(WalletError::crypto(
                "MAC mismatch: the passphrase is wrong, or the key file has been modified",
            ));
        }
        let seed_bytes = if uses_kdf {
            kdf::xor_seed(&key, &self.nonce, &self.ciphertext)
        } else {
            self.ciphertext
        };
        let seed = Secret32::from_bytes(seed_bytes);
        // A valid MAC only proves the passphrase. Re-derive the pubkey as well,
        // to catch a file whose stored pubkey doesn't match the decrypted seed.
        let derived = sig::public_key_of(&seed);
        if !ct_eq(&derived, &self.pubkey) {
            return Err(WalletError::crypto(
                "the decrypted seed does not derive the public key stored in the file",
            ));
        }
        Ok(seed)
    }

    fn mac_input(&self) -> String {
        self.render_without_mac()
    }

    fn render_without_mac(&self) -> String {
        let mut s = String::new();
        s.push_str(MAGIC);
        s.push('\n');
        s.push_str(&format!("version: {}\n", self.version));
        s.push_str(&format!("role: {}\n", self.role.as_str()));
        s.push_str(&format!("created: {}\n", self.created));
        s.push_str(&format!("kdf: {}\n", self.kdf));
        s.push_str(&format!("kdf_iters: {}\n", self.kdf_iters));
        s.push_str(&format!(
            "kdf_salt: {}\n",
            plaine_consensus::hex::encode(&self.kdf_salt)
        ));
        s.push_str(&format!("cipher: {}\n", self.cipher));
        s.push_str(&format!(
            "nonce: {}\n",
            plaine_consensus::hex::encode(&self.nonce)
        ));
        s.push_str(&format!(
            "pubkey: {}\n",
            plaine_consensus::hex::encode(&self.pubkey)
        ));
        s.push_str(&format!("address: {}\n", self.address));

        s.push_str(&format!("ciphertext: {}\n", sechex::encode(&self.ciphertext)));
        s
    }

    pub fn render(&self) -> String {
        let mut s = self.render_without_mac();
        s.push_str(&format!(
            "mac: {}\n",
            plaine_consensus::hex::encode(&self.mac)
        ));
        s
    }

    pub fn parse(text: &str) -> Result<KeyFile> {
        if !text.is_ascii() {
            return Err(WalletError::format(
                "key file contains a non-ASCII byte; the format is ASCII only",
            ));
        }
        let mut lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();

        if lines.last() == Some(&"") {
            lines.pop();
        }
        if lines.is_empty() {
            return Err(WalletError::format("key file is empty"));
        }
        if lines[0] != MAGIC {
            return Err(WalletError::format(format!(
                "line 1: expected the magic {MAGIC:?}, found {:?}",
                lines[0]
            )));
        }
        if lines.len() != 1 + FIELDS.len() {
            return Err(WalletError::format(format!(
                "key file has {} lines, expected exactly {}",
                lines.len(),
                1 + FIELDS.len()
            )));
        }
        let mut values: Vec<&str> = Vec::with_capacity(FIELDS.len());
        for (i, expected) in FIELDS.iter().enumerate() {
            let lineno = i + 2;
            let line = lines[i + 1];
            let Some((key, value)) = line.split_once(": ") else {
                return Err(WalletError::format(format!(
                    "line {lineno}: expected `{expected}: <value>`, found {line:?}"
                )));
            };
            if key != *expected {
                return Err(WalletError::format(format!(
                    "line {lineno}: expected key {expected:?}, found {key:?} \
                     (fixed order; an unknown or reordered key is an error)"
                )));
            }
            if value.is_empty() {
                return Err(WalletError::format(format!("line {lineno}: {key} is empty")));
            }
            values.push(value);
        }

        let num = |i: usize, name: &str| -> Result<u64> {
            values[i].parse::<u64>().map_err(|_| {
                WalletError::format(format!(
                    "line {}: {name} must be a decimal integer, found {:?}",
                    i + 2,
                    values[i]
                ))
            })
        };
        let bytes32 = |i: usize, name: &str| -> Result<[u8; 32]> {
            let v = sechex::decode(values[i]).map_err(|e| {
                WalletError::format(format!("line {}: {name}: {e}", i + 2))
            })?;
            v.as_slice().try_into().map_err(|_| {
                WalletError::format(format!(
                    "line {}: {name} must be 32 bytes (64 hex digits), got {}",
                    i + 2,
                    v.len()
                ))
            })
        };

        let version_field = num(0, "version")?;
        if version_field != FORMAT_VERSION as u64 {
            return Err(WalletError::format(format!(
                "line 2: key file format version {version_field}, this build understands \
                 {FORMAT_VERSION}"
            )));
        }
        let version = FORMAT_VERSION;
        let role = Role::parse(values[1]).map_err(|e| WalletError::format(format!("line 3: {e}")))?;
        let created = num(2, "created")?;
        let kdf_name = values[3].to_string();
        let kdf_iters = num(4, "kdf_iters")?;
        let kdf_salt = bytes32(5, "kdf_salt")?;
        let cipher = values[6].to_string();
        let nonce_v = sechex::decode(values[7])
            .map_err(|e| WalletError::format(format!("line 9: nonce: {e}")))?;
        let nonce: [u8; 16] = nonce_v.as_slice().try_into().map_err(|_| {
            WalletError::format(format!(
                "line 9: nonce must be 16 bytes (32 hex digits), got {}",
                nonce_v.len()
            ))
        })?;
        let pubkey = bytes32(8, "pubkey")?;
        let address = values[9].to_string();
        let ciphertext = bytes32(10, "ciphertext")?;
        let mac = bytes32(11, "mac")?;

        let derived_address = crypto::address_from_pubkey(&pubkey);
        if derived_address != address {
            return Err(WalletError::format(format!(
                "line 11: address does not match pubkey (pubkey derives {derived_address})"
            )));
        }
        if kdf_name == kdf::KDF_NONE && kdf_iters != 0 {
            return Err(WalletError::format(
                "line 6: kdf is none but kdf_iters is not 0",
            ));
        }

        if kdf_name == kdf::KDF_ARGON2ID_V1
            && !(1..=kdf::ARGON2_MAX_PASSES).contains(&kdf_iters)
        {
            return Err(WalletError::format(format!(
                "line 6: kdf_iters {kdf_iters} is out of range for {}; it counts argon2id \
                 passes, 1..={}",
                kdf::KDF_ARGON2ID_V1,
                kdf::ARGON2_MAX_PASSES
            )));
        }
        if kdf_iters > kdf::MAX_ITERS {
            return Err(WalletError::format(format!(
                "line 6: kdf_iters {kdf_iters} exceeds the ceiling of {}; at roughly one \
                 second per {} iterations, opening this file would hang with no output. \
                 That is almost always a mistyped digit. Correct the line by hand if the \
                 value is real; the mac covers it, so the passphrase was used with the \
                 same count.",
                kdf::MAX_ITERS,
                kdf::DEFAULT_ITERS
            )));
        }

        let kf = KeyFile {
            version,
            role,
            created,
            kdf: kdf_name,
            kdf_iters,
            kdf_salt,
            cipher,
            nonce,
            pubkey,
            address,
            ciphertext,
            mac,
        };

        let mut on_disk = lines.join("\n");
        on_disk.push('\n');
        // The MAC covers the canonical render, so a file that parses but is not
        // byte-for-byte canonical could never open. Reject it here and name the
        // first field that differs.
        let canonical = kf.render();
        if canonical != on_disk {
            let n = canonical
                .lines()
                .zip(on_disk.lines())
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            let field = if n == 0 { "the magic" } else { FIELDS[n - 1] };
            return Err(WalletError::format(format!(
                "line {}: {field} is not in the canonical form this tool writes \
                 (values are exact: lowercase hex, decimal with no leading zeros, one \
                 space after the colon). The mac is computed over the canonical text, so \
                 a non-canonical file will not open anyway.",
                n + 1
            )));
        }
        Ok(kf)
    }

    pub fn is_unencrypted(&self) -> bool {
        self.kdf == kdf::KDF_NONE
    }
}

pub fn read(path: &Path) -> Result<KeyFile> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| WalletError::io(format!("cannot read {}: {e}", path.display())))?;
    KeyFile::parse(&text).map_err(|e| WalletError::format(format!("{}: {e}", path.display())))
}

pub fn write_new(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                WalletError::refused(format!(
                    "{} already exists; this tool never overwrites a key file",
                    path.display()
                ))
            } else {
                WalletError::io(format!("cannot create {}: {e}", path.display()))
            }
        })?;
    f.write_all(text.as_bytes())
        .map_err(|e| WalletError::io(format!("cannot write {}: {e}", path.display())))?;
    f.flush()
        .map_err(|e| WalletError::io(format!("cannot flush {}: {e}", path.display())))?;
    Ok(())
}

pub fn create_verified(
    path: &Path,
    kf: &KeyFile,
    passphrase: Option<&SecretBytes>,
    expect_pubkey: &[u8; 32],
) -> Result<()> {
    write_new(path, &kf.render())?;
    let verified = (|| -> Result<()> {
        let reread = read(path)?;
        let seed = reread.open(passphrase)?;
        let derived = sig::public_key_of(&seed);
        if !ct_eq(&derived, expect_pubkey) {
            return Err(WalletError::crypto(
                "the file on disk does not reproduce the expected public key",
            ));
        }
        Ok(())
    })();
    match verified {
        Ok(()) => Ok(()),
        Err(e) => {
            let removed = std::fs::remove_file(path).is_ok();
            Err(WalletError::crypto(format!(
                "write-then-verify failed ({e}); {} - nothing was created",
                if removed {
                    format!("{} was deleted", path.display())
                } else {
                    format!("could not delete {} - remove it by hand", path.display())
                }
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(tag: &[u8]) -> Secret32 {
        Secret32::from_bytes(plaine_consensus::blake3::hash(tag))
    }

    fn pass(s: &str) -> SecretBytes {
        SecretBytes::from_vec(s.as_bytes().to_vec())
    }

    fn sealed() -> (KeyFile, Secret32, SecretBytes) {
        let s = seed(b"keyfile test seed");
        let p = pass("a generated passphrase, not a chosen one");
        let kf = KeyFile::seal(Role::Spend, 1_765_432_100, &s, Some(&p), 64).unwrap();
        (kf, s, p)
    }

    #[test]
    fn roundtrip_through_text() {
        let (kf, s, p) = sealed();
        let text = kf.render();
        assert!(text.starts_with("PLNEKEY1\n"));
        let parsed = KeyFile::parse(&text).unwrap();
        assert_eq!(parsed, kf);
        let out = parsed.open(Some(&p)).unwrap();
        assert_eq!(out.expose(), s.expose());
    }

    #[test]
    fn file_text_never_contains_the_seed() {
        let (kf, s, _p) = sealed();
        let text = kf.render();
        let seed_hex = sechex::encode(s.expose());
        assert!(!text.contains(&seed_hex), "the seed is in the file in the clear");
    }

    #[test]
    fn unencrypted_files_are_legal_and_labelled() {
        let s = seed(b"keyfile none");
        let kf = KeyFile::seal(Role::Spend, 1, &s, None, 0).unwrap();
        assert!(kf.is_unencrypted());
        assert_eq!(kf.kdf, kdf::KDF_NONE);
        assert_eq!(kf.kdf_iters, 0);

        assert!(kf.render().contains(&sechex::encode(s.expose())));
        assert_eq!(kf.open(None).unwrap().expose(), s.expose());
    }

    #[test]
    fn wrong_passphrase_is_rejected_by_the_mac() {
        let (kf, _s, _p) = sealed();
        let err = kf.open(Some(&pass("wrong"))).unwrap_err();
        assert_eq!(err.kind(), "crypto");
    }

    #[test]
    fn every_tampered_field_is_caught() {
        let (kf, _s, p) = sealed();
        let text = kf.render();

        let mut t = KeyFile::parse(&text).unwrap();
        t.ciphertext[0] ^= 0x01;
        assert!(t.open(Some(&p)).is_err(), "ciphertext flip must be caught");

        let mut t = KeyFile::parse(&text).unwrap();
        t.mac[31] ^= 0x80;
        assert!(t.open(Some(&p)).is_err(), "mac flip must be caught");

        let mut t = KeyFile::parse(&text).unwrap();
        t.kdf_salt[5] ^= 0xFF;
        assert!(t.open(Some(&p)).is_err(), "salt swap must be caught");

        let mut t = KeyFile::parse(&text).unwrap();
        t.kdf_iters = 1;
        assert!(t.open(Some(&p)).is_err(), "iters downgrade must be caught");

        let mut t = KeyFile::parse(&text).unwrap();
        t.nonce[0] ^= 0x0F;
        assert!(t.open(Some(&p)).is_err(), "nonce swap must be caught");

        let mut t = KeyFile::parse(&text).unwrap();
        t.role = Role::Author;
        assert!(t.open(Some(&p)).is_err(), "role edit must be caught");

        let mut t = KeyFile::parse(&text).unwrap();
        t.created += 1;
        assert!(t.open(Some(&p)).is_err(), "created edit must be caught");
    }

    #[test]
    fn seal_uses_per_key_salt_and_nonce() {
        let created = 1_765_432_100u64;
        let p = pass("a generated passphrase, not a chosen one");
        let s1 = seed(b"seal salt seed one");
        let s2 = seed(b"seal salt seed two");
        let a = KeyFile::seal(Role::Spend, created, &s1, Some(&p), 64).unwrap();
        let b = KeyFile::seal(Role::Spend, created, &s2, Some(&p), 64).unwrap();

        assert_eq!(a.kdf_salt, kdf::derive_salt(&s1, created), "salt must be the derived one");
        assert_eq!(a.nonce, kdf::derive_nonce(&s1, created), "nonce must be the derived one");
        assert_ne!(a.kdf_salt, b.kdf_salt, "two keys must not share a salt");
        assert_ne!(a.nonce, b.nonce, "two keys must not share a cipher nonce");
        assert_ne!(a.kdf_salt, [0u8; 32], "salt must not be all-zero");
        assert_ne!(a.nonce, [0u8; 16]);

        let c = KeyFile::seal(Role::Spend, created + 1, &s1, Some(&p), 64).unwrap();
        assert_ne!(a.kdf_salt, c.kdf_salt);
        assert_ne!(a.nonce, c.nonce);

        let x: Vec<u8> = a
            .ciphertext
            .iter()
            .zip(b.ciphertext.iter())
            .map(|(p, q)| p ^ q)
            .collect();
        let y: Vec<u8> = s1
            .expose()
            .iter()
            .zip(s2.expose().iter())
            .map(|(p, q)| p ^ q)
            .collect();
        assert_ne!(x, y, "ciphertext xor must not reveal seed xor");
    }

    #[test]
    fn macced_file_with_wrong_pubkey_is_refused() {
        let (kf, _s, p) = sealed();
        let mut bad = kf.clone();
        bad.pubkey[0] ^= 0xFF;
        bad.address = crypto::address_from_pubkey(&bad.pubkey);
        let key = kdf::effective_key(Some(&p), &bad.kdf_salt, bad.kdf_iters);
        bad.mac = kdf::mac(&key, bad.mac_input().as_bytes());

        assert!(ct_eq(&kdf::mac(&key, bad.mac_input().as_bytes()), &bad.mac));
        let err = bad.open(Some(&p)).unwrap_err();
        assert_eq!(err.kind(), "crypto");
        assert!(
            err.to_string().contains("does not derive the public key"),
            "must fail on the derived-key check, got: {err}"
        );

        assert!(kf.open(Some(&p)).is_ok());
    }

    #[test]
    fn open_names_bad_passphrase_kdf_and_cipher() {
        let (kf, _s, p) = sealed();

        let err = kf.open(None).unwrap_err();
        assert_eq!(err.kind(), "usage", "a missing passphrase is a usage error");
        assert!(err.to_string().contains("passphrase"), "{err}");

        let mut k = kf.clone();
        k.kdf = "scrypt-v1".to_string();
        let err = k.open(Some(&p)).unwrap_err();
        assert_eq!(err.kind(), "format");
        assert!(err.to_string().contains("unsupported kdf"), "{err}");

        let mut k = kf.clone();
        k.cipher = "rot13".to_string();
        let err = k.open(Some(&p)).unwrap_err();
        assert_eq!(err.kind(), "format");
        assert!(err.to_string().contains("unsupported cipher"), "{err}");

        let u = KeyFile::seal(Role::Spend, 1, &seed(b"open guards none"), None, 0).unwrap();
        assert_eq!(u.open(Some(&p)).unwrap_err().kind(), "usage");
    }

    #[test]
    fn version_that_wraps_u32_is_refused() {
        let (kf, _s, _p) = sealed();
        let good = kf.render();
        for bogus in ["4294967297", "8589934593", "18446744069414584321", "2", "0"] {
            let text = good.replacen("version: 1\n", &format!("version: {bogus}\n"), 1);
            let err = KeyFile::parse(&text)
                .map(|k| k.version)
                .expect_err(&format!("version {bogus} must be refused"));
            assert_eq!(err.kind(), "format");
            assert!(err.to_string().contains(bogus), "the refusal must name the value: {err}");
        }
        assert!(KeyFile::parse(&good).is_ok(), "and version 1 still parses");
    }

    #[test]
    fn only_the_canonical_text_parses() {
        let (kf, _s, p) = sealed();
        let good = kf.render();

        let cases: Vec<(&str, String)> = vec![
            ("trailing junk line", format!("{good}anything at all\n")),
            ("two junk lines", format!("{good}a\nb\n")),
            ("leading zeros on version", good.replacen("version: 1\n", "version: 01\n", 1)),
            (
                "leading zeros on created",
                good.replacen(
                    &format!("created: {}", kf.created),
                    &format!("created: 0{}", kf.created),
                    1,
                ),
            ),
            (
                "uppercase hex",
                good.replacen(
                    &plaine_consensus::hex::encode(&kf.pubkey),
                    &plaine_consensus::hex::encode(&kf.pubkey).to_uppercase(),
                    1,
                ),
            ),
        ];
        for (name, text) in cases {
            let err = KeyFile::parse(&text).err().unwrap_or_else(|| panic!("{name} must not parse"));
            assert_eq!(err.kind(), "format", "{name}");
        }

        let crlf = good.replace('\n', "\r\n");
        let back = KeyFile::parse(&crlf).expect("CRLF must still parse");
        assert_eq!(back, kf);
        assert_eq!(back.open(Some(&p)).unwrap().expose(), _s.expose());
    }

    #[test]
    fn absurd_iteration_count_is_refused() {
        let (kf, _s, _p) = sealed();
        let good = kf.render();
        for bogus in [kdf::MAX_ITERS + 1, u64::MAX / 2, u64::MAX] {
            let text = good.replacen(
                &format!("kdf_iters: {}", kf.kdf_iters),
                &format!("kdf_iters: {bogus}"),
                1,
            );
            let err = KeyFile::parse(&text).expect_err("must be refused");
            assert_eq!(err.kind(), "format");
            assert!(err.to_string().contains("kdf_iters"), "{err}");
            assert!(err.to_string().contains(&bogus.to_string()), "{err}");
        }

        let text = good.replacen(
            &format!("kdf_iters: {}", kf.kdf_iters),
            &format!("kdf_iters: {}", kdf::MAX_ITERS),
            1,
        );
        assert!(KeyFile::parse(&text).is_ok(), "the ceiling value must parse");
    }

    #[test]
    fn kdf_none_with_nonzero_iters_is_refused() {
        let s = seed(b"keyfile none iters");
        let kf = KeyFile::seal(Role::Spend, 1, &s, None, 0).unwrap();
        let text = kf.render().replacen("kdf_iters: 0", "kdf_iters: 1024", 1);
        let err = KeyFile::parse(&text).expect_err("must be refused");
        assert_eq!(err.kind(), "format");
        assert!(err.to_string().contains("kdf_iters"), "{err}");
    }

    #[test]
    fn non_ascii_byte_is_refused() {
        let (kf, _s, _p) = sealed();
        let text = kf.render().replacen("role: spend", "role: sp\u{00e9}nd", 1);
        let err = KeyFile::parse(&text).expect_err("must be refused");
        assert_eq!(err.kind(), "format");
        assert!(
            err.to_string().contains("non-ASCII"),
            "the ASCII gate must be what fires, got: {err}"
        );
    }

    #[test]
    fn kdf_downgrade_to_none_is_caught() {
        let (kf, _s, _p) = sealed();
        let text = kf
            .render()
            .replace("kdf: blake3-iter-v1", "kdf: none")
            .replace(&format!("kdf_iters: {}", kf.kdf_iters), "kdf_iters: 0");
        let parsed = KeyFile::parse(&text).unwrap();

        let err = parsed.open(None).unwrap_err();
        assert_eq!(err.kind(), "crypto");
    }

    #[test]
    fn parser_is_strict() {
        let (kf, _s, _p) = sealed();
        let good = kf.render();

        let cases: Vec<(&str, String)> = vec![
            ("bad magic", good.replacen("PLNEKEY1", "PLNEKEY2", 1)),
            ("missing line", good.replacen(&format!("role: {}\n", kf.role.as_str()), "", 1)),
            (
                "duplicate line",
                good.replacen("created:", "created: 1\ncreated:", 1),
            ),
            (
                "unknown key",
                good.replacen("cipher:", "ciphertext_extra:", 1),
            ),
            (
                "reordered",
                good.replacen("version: 1\nrole:", "role_first: x\nversion: 1\nrole:", 1),
            ),
            ("odd hex", good.replacen("mac: ", "mac: a", 1)),
            ("non-ascii", good.replacen("role: ", "role: \u{00e9}", 1)),
            ("no separator", good.replacen("cipher: ", "cipher=", 1)),
        ];
        for (name, text) in cases {
            let r = KeyFile::parse(&text);
            assert!(r.is_err(), "{name} should not parse");
            assert_eq!(r.unwrap_err().kind(), "format", "{name}");
        }
    }

    #[test]
    fn address_must_agree_with_pubkey() {
        let (kf, _s, _p) = sealed();
        let other = crypto::address_from_pubkey(&[0x11u8; 32]);
        let text = kf.render().replace(&kf.address, &other);
        let err = KeyFile::parse(&text).unwrap_err();
        assert!(err.to_string().contains("address does not match pubkey"));
    }

    #[test]
    fn write_new_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("plnkf-{}", crate::now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.plnekey");
        write_new(&path, "first").unwrap();
        let before = std::fs::read(&path).unwrap();
        let err = write_new(&path, "second").unwrap_err();
        assert_eq!(err.kind(), "refused");
        assert!(err.to_string().contains(&path.display().to_string()));
        assert_eq!(std::fs::read(&path).unwrap(), before, "file must be untouched");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_verified_rolls_back_on_a_bad_file() {
        let dir = std::env::temp_dir().join(format!("plnrb-{}", crate::now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollback.plnekey");
        let (mut kf, _s, p) = sealed();

        kf.ciphertext[0] ^= 0xFF;
        let err = create_verified(&path, &kf, Some(&p), &kf.pubkey).unwrap_err();
        assert!(err.to_string().contains("nothing was created"), "{err}");
        assert!(!path.exists(), "the rolled-back file must not exist");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_verified_succeeds_on_a_good_file() {
        let dir = std::env::temp_dir().join(format!("plnok-{}", crate::now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ok.plnekey");
        let (kf, _s, p) = sealed();
        create_verified(&path, &kf, Some(&p), &kf.pubkey).unwrap();
        assert!(path.exists());
        assert_eq!(read(&path).unwrap(), kf);
        std::fs::remove_dir_all(&dir).ok();
    }
}
