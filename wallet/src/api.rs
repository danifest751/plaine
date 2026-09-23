//! The wallet as a library: what `plaine-wallet` does, without a terminal.
//!
//! Nothing here prints or reads stdin. Advice the command line prints comes back as
//! [`Notice`]s for the caller to show however it shows things; the CLI prints them
//! word for word, a GUI puts them in a dialog. Secrets stay in [`Secret32`] and
//! [`SecretBytes`], which zero themselves, and an [`OpenKey`] drops its seed with it.

use std::path::{Path, PathBuf};

use crate::error::{Result, WalletError};
use crate::keyfile::{self, KeyFile, Role};
use crate::secret::{Secret32, SecretBytes};
use crate::{amount, journal, kdf, sechex, sig, txbuild};
pub use crate::kdf::Kdf;
use plaine_consensus::constants::Network;

/// Advice that goes with a result. The text is what the CLI prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The key file holds the seed in the clear (`kdf: none`).
    Unencrypted,
    /// The KDF work factor is below the recommended floor.
    FewIterations { iters: u64 },
    /// The passphrase KDF is iterated, not memory-hard.
    KdfNotMemoryHard,
    /// Keep the passphrase apart from the seed material.
    KeepPassphraseApart,
    /// Author and checkpoint keys belong on an air-gapped machine.
    AirGapThisRole { memory_hard: bool },
    /// The seed came without a checksum; nothing verified the transcription.
    UncheckedSeed,
    /// Opening this file takes several times the default KDF work.
    SlowToOpen { iters: u64 },
}

impl Notice {
    /// The notice as lines of text, the way the CLI prints it.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Notice::Unencrypted => vec![
                "kdf: none - the seed is stored in this file in the clear.".into(),
                "Anyone who reads the file has the key. It is allowed on purpose".into(),
                "and labelled as such.".into(),
            ],
            Notice::FewIterations { iters } => vec![format!(
                "warning: kdf_iters {iters} is below the recommended {} ",
                kdf::WARN_BELOW_ITERS
            )],
            Notice::KdfNotMemoryHard => vec![
                "note: the KDF (blake3-iter-v1) is not memory-hard. Iterations raise the \
                 cost per guess linearly; they do not slow down a GPU."
                    .into(),
            ],
            Notice::KeepPassphraseApart => vec![
                "note: use a generated passphrase, and keep it apart from the seed material. \
                 Anyone holding both has this file in plaintext."
                    .into(),
            ],
            Notice::AirGapThisRole { memory_hard } => vec![
                "This is an author or checkpoint key. Generate it and keep it air-gapped.".into(),
                "For these roles what protects the key is keeping the file out of reach.".into(),
                if *memory_hard {
                    "A memory-hard KDF slows guessing; it does not replace keeping the file away."
                } else {
                    "The KDF here is not memory-hard, so do not lean on the passphrase."
                }
                .into(),
            ],
            Notice::UncheckedSeed => vec![
                "note: this seed carries no checksum (64 digits, not 68), so nothing verified \
                 the transcription. Check the address printed below against the one you \
                 expect before you send anything to it."
                    .into(),
            ],
            Notice::SlowToOpen { iters } => vec![format!(
                "note: this key file uses {iters} KDF iterations ({}x the default of {}). \
                 Deriving the key prints nothing until it finishes; this is not a hang.",
                iters / kdf::DEFAULT_ITERS,
                kdf::DEFAULT_ITERS
            )],
        }
    }

    /// Whether the CLI frames this notice as a block rather than a single line.
    pub fn is_block(&self) -> bool {
        matches!(self, Notice::Unencrypted | Notice::AirGapThisRole { .. })
    }
}

/// The public part of a key file: everything that can be shown without a passphrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySummary {
    pub role: Role,
    pub created: u64,
    pub kdf: String,
    pub kdf_iters: u64,
    pub cipher: String,
    pub pubkey: [u8; 32],
    pub address: String,
    pub encrypted: bool,
}

impl KeySummary {
    pub fn of(kf: &KeyFile) -> KeySummary {
        KeySummary {
            role: kf.role,
            created: kf.created,
            kdf: kf.kdf.clone(),
            kdf_iters: kf.kdf_iters,
            cipher: kf.cipher.clone(),
            pubkey: kf.pubkey,
            address: kf.address.clone(),
            encrypted: !kf.is_unencrypted(),
        }
    }
}

/// Reads a key file's public part. No passphrase is needed.
pub fn inspect(path: &Path) -> Result<KeySummary> {
    Ok(KeySummary::of(&keyfile::read(path)?))
}

/// The notices that go with opening this key file, before the passphrase is asked for.
pub fn open_notices(summary: &KeySummary) -> Vec<Notice> {
    let mut out = Vec::new();
    if summary.encrypted && summary.kdf_iters > kdf::NOTICE_ABOVE_ITERS {
        out.push(Notice::SlowToOpen { iters: summary.kdf_iters });
    }
    out
}

/// Decodes seed material typed or pasted by a person: the 68-character backup string,
/// or, when not `restoring`, 64 bare hex digits.
///
/// Restoring refuses the bare form: without its checksum nothing catches a
/// one-character slip, and a slip restores a different, empty wallet.
pub fn decode_seed_text(text: &str, restoring: bool) -> Result<(Secret32, Option<Notice>)> {
    let (seed, source) = sechex::decode_backup(text.trim())?;
    let notice = if source == sechex::SeedSource::Unchecked {
        if restoring {
            return Err(WalletError::refused(
                "this is 64 bare hex digits, not the 68-digit checksummed string that \
                 `backup` prints, so nothing here can catch a one-character slip, and a \
                 slip restores a different, empty wallet. Paste the whole backup line, \
                 checksum included. If this really is raw entropy or a backup from before \
                 checksums existed, use `new` with the same flags; it takes either form \
                 and produces the identical key file.",
            ));
        }
        Some(Notice::UncheckedSeed)
    } else {
        None
    };
    crate::secret::check_seed_material(seed.expose())?;
    Ok((seed, notice))
}

/// A key file written by [`create_key`].
#[derive(Debug)]
pub struct Created {
    pub summary: KeySummary,
    /// The seed was minted here from the OS CSPRNG, so no backup of it exists yet.
    pub generated: bool,
    pub notices: Vec<Notice>,
}

/// Writes a new key file at `path` and proves it opens.
///
/// `seed` is `None` to mint one from the OS CSPRNG, or seed material from
/// [`decode_seed_text`]. With a passphrase the file is sealed under `with`; new keys
/// should use [`Kdf::RECOMMENDED`] (argon2id). The file is written, read back and
/// opened with the same passphrase before this returns; if that fails, nothing is
/// left behind. An existing file is never overwritten.
pub fn create_key(
    path: &Path,
    role: Role,
    seed: Option<Secret32>,
    passphrase: Option<&SecretBytes>,
    with: Kdf,
) -> Result<Created> {
    if path.exists() {
        return Err(WalletError::refused(format!(
            "{} already exists; this tool never overwrites a key file",
            path.display()
        )));
    }
    with.check()?;
    let generated = seed.is_none();
    let seed = match seed {
        Some(s) => s,
        None => crate::rng::generate_seed()?,
    };
    let kf = KeyFile::seal_with(role, crate::now_secs(), &seed, passphrase, with)?;
    keyfile::create_verified(path, &kf, passphrase, &kf.pubkey)?;
    let notices = create_notices(role, passphrase.is_some(), with);
    Ok(Created { summary: KeySummary::of(&kf), generated, notices })
}

/// The advice that goes with creating a key of `role`, with or without a passphrase,
/// sealed under `with`.
pub fn create_notices(role: Role, encrypted: bool, with: Kdf) -> Vec<Notice> {
    let mut out = Vec::new();
    if !encrypted {
        out.push(Notice::Unencrypted);
    } else {
        if let Kdf::Blake3Iter { iters } = with {
            if iters < kdf::WARN_BELOW_ITERS {
                out.push(Notice::FewIterations { iters });
            }
            out.push(Notice::KdfNotMemoryHard);
        }
        out.push(Notice::KeepPassphraseApart);
    }
    if role == Role::Author || role == Role::Checkpoint {
        out.push(Notice::AirGapThisRole { memory_hard: encrypted && with.is_memory_hard() });
    }
    out
}

/// Refuses a `blake3-iter-v1` work factor above [`kdf::MAX_ITERS`].
pub fn check_iters(iters: u64) -> Result<()> {
    Kdf::Blake3Iter { iters }.check()
}

/// An opened key: the seed, decrypted and checked against the file's public key.
/// Dropping it zeroes the seed; a GUI drops it to lock.
pub struct OpenKey {
    path: PathBuf,
    file: KeyFile,
    seed: Secret32,
}

impl std::fmt::Debug for OpenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenKey")
            .field("path", &self.path)
            .field("address", &self.file.address)
            .finish_non_exhaustive()
    }
}

/// Opens the key file at `path`. `passphrase` must be `None` exactly when the file
/// is not encrypted. The decrypted seed has to reproduce the stored public key.
pub fn open(path: &Path, passphrase: Option<&SecretBytes>) -> Result<OpenKey> {
    let file = keyfile::read(path)?;
    let seed = file.open(passphrase)?;
    if !crate::secret::ct_eq(&sig::public_key_of(&seed), &file.pubkey) {
        return Err(WalletError::crypto("derived public key does not match the file"));
    }
    Ok(OpenKey { path: path.to_path_buf(), file, seed })
}

/// A transfer signed by [`OpenKey::sign_transfer`], ready for `tx_sendRaw`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedTransfer {
    pub txid: [u8; 32],
    /// The wire bytes.
    pub raw: Vec<u8>,
    /// `raw` as lowercase hex, the form `tx_sendRaw` takes.
    pub hex: String,
    pub from: String,
    pub to: String,
    pub amount: u128,
    pub fee: u128,
    pub nonce: u64,
}

impl OpenKey {
    pub fn summary(&self) -> KeySummary {
        KeySummary::of(&self.file)
    }

    pub fn address(&self) -> &str {
        &self.file.address
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Signs a transfer of `amount` mile to `to`, paying `fee`, at `nonce`. The
    /// signature is verified and the encoding round-tripped before this returns.
    pub fn sign_transfer(
        &self,
        network: Network,
        to: &str,
        amount: u128,
        fee: u128,
        nonce: u64,
    ) -> Result<SignedTransfer> {
        let tx = txbuild::build_transfer(network, &self.seed, to, amount, fee, nonce)?;
        let raw = tx.encode().to_vec();
        Ok(SignedTransfer {
            txid: tx.txid(),
            hex: plaine_consensus::hex::encode(&raw),
            raw,
            from: self.file.address.clone(),
            to: to.to_string(),
            amount,
            fee,
            nonce,
        })
    }

    /// The 68-character backup string: the seed and a 4-digit checksum. It is the
    /// private key; show it only on an explicit request.
    pub fn backup_string(&self) -> String {
        sechex::encode_backup(&self.seed)
    }

    /// Writes this key to a new file at `out` under a new passphrase (or none), and
    /// carries the announcement journal across. The original file is left untouched.
    /// Returns the new file's summary and how many journal entries were carried.
    pub fn rewrap(
        &self,
        out: &Path,
        passphrase: Option<&SecretBytes>,
        with: Kdf,
    ) -> Result<(KeySummary, Option<usize>)> {
        if out == self.path {
            return Err(WalletError::refused(
                "--in and --out must differ: this command writes a new file and leaves \
                 the original untouched. Remove the old one by hand once you have verified \
                 the new one.",
            ));
        }
        if out.exists() {
            return Err(WalletError::refused(format!(
                "{} already exists; this tool never overwrites a key file",
                out.display()
            )));
        }
        let jsrc = journal::path_for(&self.path);
        let jdst = journal::path_for(out);
        if jdst.exists() {
            return Err(WalletError::refused(format!(
                "{} already exists but {} does not; that journal belongs to a key \
                 file that is gone. Move it aside by hand - this command must carry \
                 {}'s journal across, and it never overwrites one.",
                jdst.display(),
                out.display(),
                self.path.display()
            )));
        }
        with.check()?;
        let newkf =
            KeyFile::seal_with(self.file.role, crate::now_secs(), &self.seed, passphrase, with)?;
        keyfile::create_verified(out, &newkf, passphrase, &self.file.pubkey)?;
        let carried = carry_journal(&jsrc, &jdst)?;
        Ok((KeySummary::of(&newkf), carried))
    }
}

/// Copies the announcement journal from `src` to `dst`. `None` when there is none.
fn carry_journal(src: &Path, dst: &Path) -> Result<Option<usize>> {
    if !src.exists() {
        return Ok(None);
    }
    let entries = journal::read(src)?;
    std::fs::copy(src, dst).map_err(|e| {
        WalletError::io(format!(
            "the new key file was written, but its announcement journal could not be \
             copied from {} to {}: {e}. Copy it by hand before signing anything with the \
             new file: without it the nonce journal is empty and a second announcement \
             at an already-used nonce will not be refused.",
            src.display(),
            dst.display()
        ))
    })?;
    Ok(Some(entries.len()))
}

/// Parses an amount with its unit, as the CLI takes it ("1.5plne", "1500000mile"),
/// into mile. The unit is not optional: the two differ by a factor of 10^6.
pub fn parse_amount(text: &str) -> Result<u128> {
    amount::parse_mile(text.trim())
}

/// Parses a number of PLNE typed into a field that is labelled PLNE ("1.5",
/// "0.000001") into mile.
pub fn parse_plne(text: &str) -> Result<u128> {
    let t = text.trim();
    if t.is_empty() || t.chars().any(|c| c.is_ascii_alphabetic()) {
        return Err(WalletError::usage(format!(
            "{t:?} is not a number of PLNE; write digits with at most one decimal point"
        )));
    }
    amount::parse_mile(&format!("{t}plne"))
}

/// Formats mile as PLNE with every significant decimal.
pub fn format_plne(mile: u128) -> String {
    amount::format_plne(mile)
}

/// Checks that `text` is a Plaine address, checksum included; for a field that is
/// checked as it is typed.
pub fn check_address(text: &str) -> Result<()> {
    plaine_consensus::crypto::decode_address(text.trim())
        .map(|_| ())
        .map_err(|e| WalletError::format(format!("not a valid Plaine address: {e}")))
}

/// The fee floor consensus allows, and whether `fee` sits exactly on it.
pub fn fee_is_at_the_floor(fee: u128) -> bool {
    txbuild::fee_is_at_the_floor(fee)
}
