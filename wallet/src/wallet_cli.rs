use crate::amount;
use crate::api;
use crate::args::{self, Parsed, Spec};
use crate::error::{Result, WalletError};
use crate::genesis;
use crate::journal;
use crate::kdf;
use crate::keyfile::{self, KeyFile, Role};
use crate::sanitize;
use crate::secret::Secret32;
use crate::txbuild;
use crate::ui::{self, PassMode, Streams};
use plaine_consensus::codec::{AnnouncementTx, TransferTx};
use plaine_consensus::constants::{FEE_FLOOR_MILE, Network};
use plaine_consensus::crypto;
use std::path::Path;

pub fn run(argv: &[String], s: &mut Streams) -> i32 {
    match dispatch(argv, s) {
        Ok(()) => 0,
        Err(e) => {
            s.warn(&format!("plaine-wallet: {e}"));
            e.exit_code()
        }
    }
}

fn dispatch(argv: &[String], s: &mut Streams) -> Result<()> {
    let Some(cmd) = argv.first() else {
        s.say(USAGE);
        return Err(WalletError::usage("no subcommand given"));
    };
    let rest = &argv[1..];
    match cmd.as_str() {
        "help" | "--help" | "-h" => {
            s.say(USAGE);
            #[cfg(feature = "author-tools")]
            s.say(ANNOUNCE_USAGE);
            Ok(())
        }
        "version" | "--version" => {
            s.say(&format!("plaine-wallet {}", env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        "new" => cmd_new(rest, s),
        "import" => cmd_import(rest, s),
        "inspect" => cmd_inspect(rest, s),
        "address" => cmd_address(rest, s),
        "verify" => cmd_verify(rest, s),
        "backup" => cmd_backup(rest, s),
        "passphrase" => cmd_passphrase(rest, s),
        "transfer" => cmd_transfer(rest, s),
        #[cfg(feature = "author-tools")]
        "announce" => cmd_announce(rest, s),
        "decode" => cmd_decode(rest, s),
        other => {
            s.say(USAGE);
            Err(WalletError::usage(format!("unknown subcommand {other:?}")))
        }
    }
}

pub const USAGE: &str = "\
plaine-wallet - the one place a Plaine private key lives.

  The wallet is offline. It opens no sockets. Balances, nonces and broadcasting
  are noded's job, so --nonce and --fee are required and have no defaults.

KEYS
  new         --out <path> --role spend|author|checkpoint
              (--seed-stdin | --seed-file <p>)
              (--passphrase-file <p> | --passphrase-stdin | --no-passphrase)
              [--kdf argon2id|blake3-iter-v1] [--kdf-iters <n>]
  import      same flags as new; restores a key from a backup
  inspect     --in <path>                       (no passphrase)
  address     --in <path>                       (no passphrase; prints only the address)
  verify      --in <path> <passphrase source>   (check the passphrase still opens it)
  passphrase  --in <old> --out <new> --old-passphrase-file <p> --new-passphrase-file <p>
              [--kdf argon2id|blake3-iter-v1] [--kdf-iters <n>]
  backup      --in <path> <passphrase source> --i-understand-this-prints-a-secret

TRANSACTIONS
  transfer    --in <key> --to <plne1...>
              --amount <n>plne|<n>mile --fee <n>mile --nonce <n>
              [--out <file>] <passphrase source>
  decode      (--in <file> | --hex <h>)
              (no key needed; checks signatures)

PASSPHRASE SOURCES
  --passphrase-file <path>   recommended
  --passphrase-stdin         piped; on new/import it is read twice and compared
  --no-passphrase            kdf: none, seed stored in the clear, labelled as such

  There is no interactive prompt: suppressing terminal echo needs a dependency
  or unsafe FFI, and this crate uses neither. Pipe the passphrase in.

SEED SOURCES
  --seed-stdin | --seed-file <path>
      new     64 hex digits (raw entropy) or the 68-digit backup string
      import  the 68-digit backup string: the last 4 digits are a checksum,
              and an unchecked restore onto a different wallet looks the same
              as lost coins

  With no seed source, `new` draws a fresh 32-byte key from the OS CSPRNG
  (getrandom(2) on Linux, BCryptGenRandom on Windows) and aborts rather than
  fall back to anything weaker. --seed-stdin/--seed-file stay for supplying
  your own entropy:
      openssl rand -hex 32 | plaine-wallet new --seed-stdin --out k.plnekey ...
";

#[cfg(feature = "author-tools")]
const ANNOUNCE_USAGE: &str = "\
AUTHOR TOOLS (this build was compiled with --features author-tools)
  announce    --in <key> --author-pubkey <64 hex>
              --payload-file <f> --encoding <n> --fee <n>mile --nonce <n>
              [--out <file>] [--reuse-nonce] <passphrase source>

  Signs a SPEC 4.1 author announcement. The public wallet leaves this out; it
  is present only in a build with --features author-tools. `decode` reads and
  checks an announcement (type 0x02) in every build.
";

const CREATE_SPEC: Spec = Spec {
    values: &["out", "role", "seed-file", "passphrase-file", "kdf", "kdf-iters"],
    switches: &["seed-stdin", "passphrase-stdin", "no-passphrase"],
};

fn cmd_new(argv: &[String], s: &mut Streams) -> Result<()> {
    create_key(argv, s, "new")
}

fn cmd_import(argv: &[String], s: &mut Streams) -> Result<()> {
    create_key(argv, s, "import")
}

fn create_key(argv: &[String], s: &mut Streams, verb: &str) -> Result<()> {
    let a = args::parse(argv, &CREATE_SPEC)?;
    let out = a.require("out")?.to_string();
    let path = Path::new(&out);
    if path.exists() {
        return Err(WalletError::refused(format!(
            "{out} already exists; this tool never overwrites a key file"
        )));
    }
    let role = Role::parse(a.require("role")?)?;
    let with = kdf_flag(&a)?;

    // `new` with no seed source mints one from the OS CSPRNG. `import` always
    // needs the backup string it is restoring, so it never generates.
    let seed_supplied = a.has("seed-stdin") || a.get("seed-file").is_some();
    let seed = if verb == "new" && !seed_supplied {
        None
    } else {
        Some(ui::seed(&a, s, verb == "import")?)
    };
    let pass = ui::passphrase(&a, s, PassMode::Create)?;

    for n in api::create_notices(role, pass.is_some(), with) {
        print_notice(s, &n);
    }
    let created = api::create_key(path, role, seed, pass.as_ref(), with)?;

    s.say(&format!("{verb}: created {out}"));
    print_public_summary(s, &created.summary);
    s.say("");
    if created.generated {
        s.say("The seed was generated from the OS CSPRNG and never printed. It exists only");
        s.say("inside the key file above. Back it up before you rely on this address:");
        s.say(&format!(
            "  plaine-wallet backup --in {out} --i-understand-this-prints-a-secret"
        ));
    } else {
        s.say("The seed was not printed. It came from you; you already have it.");
        s.say("To print it on purpose: plaine-wallet backup --in <path> --i-understand-this-prints-a-secret");
    }
    Ok(())
}

/// Prints a notice from the library the way this tool always has.
fn print_notice(s: &mut Streams, n: &api::Notice) {
    let lines = n.lines();
    if n.is_block() {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        s.warn_block(&refs);
    } else {
        for l in &lines {
            s.warn(l);
        }
    }
}

fn print_public_summary(s: &mut Streams, kf: &api::KeySummary) {
    s.say(&format!("  role       {}", kf.role.as_str()));
    s.say(&format!("  created    {}", kf.created));
    s.say(&format!("  kdf        {} (iters {})", kf.kdf, kf.kdf_iters));
    s.say(&format!("  cipher     {}", kf.cipher));
    s.say(&format!(
        "  pubkey     {}",
        plaine_consensus::hex::encode(&kf.pubkey)
    ));
    s.say(&format!("  address    {}", kf.address));
}

const IN_ONLY_SPEC: Spec = Spec {
    values: &["in"],
    switches: &[],
};

fn cmd_inspect(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &IN_ONLY_SPEC)?;
    let path = a.require("in")?;
    let kf = keyfile::read(Path::new(path))?;
    s.say(&format!("inspect: {path}"));
    s.say(&format!("  magic      {}", keyfile::MAGIC));
    s.say(&format!("  version    {}", kf.version));
    print_public_summary(s, &api::KeySummary::of(&kf));
    if kf.is_unencrypted() {
        s.warn_block(&["kdf: none - this file holds the seed in the clear."]);
    } else if kf.kdf == kdf::KDF_ARGON2ID_V1 {
        s.say("  note       argon2id-v1 costs 64 MiB of memory per guess");
    } else {
        s.say("  note       blake3-iter-v1 is not memory-hard");
    }
    Ok(())
}

fn cmd_address(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &IN_ONLY_SPEC)?;
    let kf = keyfile::read(Path::new(a.require("in")?))?;
    s.say(&kf.address);
    Ok(())
}

const OPEN_SPEC: Spec = Spec {
    values: &["in", "passphrase-file"],
    switches: &["passphrase-stdin", "no-passphrase"],
};

fn cmd_verify(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &OPEN_SPEC)?;
    let path = a.require("in")?;
    let kf = keyfile::read(Path::new(path))?;
    let pass = open_passphrase(&a, s, &kf)?;
    let key = api::open(Path::new(path), pass.as_ref())?;
    s.say(&format!("verify: OK  {path}"));
    s.say(&format!("  address    {}", key.address()));
    s.say("  the passphrase opens this file and the seed derives the stored public key");
    Ok(())
}

fn open_passphrase(
    a: &Parsed,
    s: &mut Streams,
    kf: &KeyFile,
) -> Result<Option<crate::secret::SecretBytes>> {
    if kf.is_unencrypted() {
        if a.get("passphrase-file").is_some() || a.has("passphrase-stdin") {
            return Err(WalletError::usage(
                "this key file has kdf: none and is not encrypted; do not pass a passphrase",
            ));
        }
        s.warn("warning: this key file is not encrypted (kdf: none)");
        return Ok(None);
    }
    kdf_cost_notice(s, kf);
    ui::passphrase(a, s, PassMode::Open)
}

const BACKUP_SPEC: Spec = Spec {
    values: &["in", "passphrase-file"],
    switches: &[
        "passphrase-stdin",
        "no-passphrase",
        "i-understand-this-prints-a-secret",
    ],
};

fn cmd_backup(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &BACKUP_SPEC)?;
    if !a.has("i-understand-this-prints-a-secret") {
        return Err(WalletError::refused(
            "backup prints the private seed to stdout. Re-run with \
             --i-understand-this-prints-a-secret if that is what you want.",
        ));
    }
    let path = a.require("in")?;
    let kf = keyfile::read(Path::new(path))?;
    let pass = open_passphrase(&a, s, &kf)?;
    let key = api::open(Path::new(path), pass.as_ref())?;
    s.warn_block(&[
        "The next line is a private key.",
        "It is now in your terminal scrollback and in anything recording this session.",
        "Write it on paper - all 68 characters - then close this terminal.",
    ]);
    s.say(&key.backup_string());
    drop(key);
    s.warn_block(&[
        "The last 4 characters are a checksum over the other 64. `import` checks it and",
        "refuses a string that does not match, so a one-character slip is caught instead",
        "of restoring a different, empty wallet. Copy the whole line.",
        "Restore with:  plaine-wallet import --seed-stdin --out <path> --role <role>",
        "Store the passphrase apart from this seed. Together they are plaintext.",
    ]);
    Ok(())
}

const PASSPHRASE_SPEC: Spec = Spec {
    values: &["in", "out", "old-passphrase-file", "new-passphrase-file", "kdf", "kdf-iters"],
    switches: &["old-no-passphrase", "new-no-passphrase"],
};

fn cmd_passphrase(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &PASSPHRASE_SPEC)?;
    let in_path = a.require("in")?.to_string();
    let out_path = a.require("out")?.to_string();
    if in_path == out_path {
        return Err(WalletError::refused(
            "--in and --out must differ: this command writes a new file and leaves \
             the original untouched. Remove the old one by hand once you have verified \
             the new one.",
        ));
    }
    if Path::new(&out_path).exists() {
        return Err(WalletError::refused(format!(
            "{out_path} already exists; this tool never overwrites a key file"
        )));
    }

    let jsrc = journal::path_for(Path::new(&in_path));
    let jdst = journal::path_for(Path::new(&out_path));
    if jdst.exists() {
        return Err(WalletError::refused(format!(
            "{} already exists but {out_path} does not; that journal belongs to a key \
             file that is gone. Move it aside by hand - this command must carry \
             {}'s journal across, and it never overwrites one.",
            jdst.display(),
            in_path
        )));
    }
    let kf = keyfile::read(Path::new(&in_path))?;
    kdf_cost_notice(s, &kf);

    let old = read_named_passphrase(&a, "old", kf.is_unencrypted())?;
    let key = api::open(Path::new(&in_path), old.as_ref())?;
    let new = read_named_passphrase(&a, "new", a.has("new-no-passphrase"))?;
    let with = kdf_flag(&a)?;

    let (summary, carried) = key.rewrap(Path::new(&out_path), new.as_ref(), with)?;
    drop(key);

    s.say(&format!("passphrase: wrote {out_path}"));
    print_public_summary(s, &summary);
    report_journal(s, &jsrc, &jdst, carried);
    s.say("");
    s.say(&format!(
        "{in_path} was not modified. Verify the new file, then remove the old one yourself:"
    ));
    s.say(&format!(
        "  plaine-wallet verify --in {out_path} --passphrase-file <new passphrase file>"
    ));
    Ok(())
}

fn report_journal(s: &mut Streams, src: &Path, dst: &Path, carried: Option<usize>) {
    let Some(n) = carried else {
        s.say(&format!("  journal    none beside {}", src.display()));
        return;
    };
    s.say(&format!(
        "  journal    carried {} entr{} to {}",
        n,
        if n == 1 { "y" } else { "ies" },
        dst.display()
    ));
    s.warn(&format!(
        "note: the rotated key holds the same seed, so it is the same account and the \
         same nonce space. Its announcement journal was copied from {}; use one key file \
         at a time, or the two journals diverge and neither sees the other's nonces.",
        src.display()
    ));
}

/// `--kdf` and `--kdf-iters`. The default stays `blake3-iter-v1`, which upstream's
/// wallet also reads; `--kdf argon2id` seals under the memory-hard KDF, and then
/// `--kdf-iters` counts its passes.
fn kdf_flag(a: &Parsed) -> Result<kdf::Kdf> {
    match a.get("kdf") {
        None | Some("blake3-iter-v1") => Ok(kdf::Kdf::Blake3Iter { iters: iters_flag(a)? }),
        Some("argon2id") | Some("argon2id-v1") => {
            let passes = match a.get("kdf-iters") {
                Some(v) => args::parse_u64("kdf-iters", v)?,
                None => kdf::ARGON2_DEFAULT_PASSES,
            };
            let with = kdf::Kdf::Argon2id { passes };
            with.check()?;
            Ok(with)
        }
        Some(other) => Err(WalletError::usage(format!(
            "--kdf {other:?} is not a KDF this wallet writes; use argon2id (memory-hard, \
             recommended) or blake3-iter-v1 (the default, readable by older wallets)"
        ))),
    }
}

pub(crate) fn iters_flag(a: &Parsed) -> Result<u64> {
    let iters = match a.get("kdf-iters") {
        Some(v) => args::parse_u64("kdf-iters", v)?,
        None => return Ok(kdf::DEFAULT_ITERS),
    };
    api::check_iters(iters)?;
    Ok(iters)
}

fn kdf_cost_notice(s: &mut Streams, kf: &KeyFile) {
    // printed whether or not the file is encrypted, as it always was
    if kf.kdf_iters > kdf::NOTICE_ABOVE_ITERS {
        print_notice(s, &api::Notice::SlowToOpen { iters: kf.kdf_iters });
    }
}

fn read_named_passphrase(
    a: &Parsed,
    which: &str,
    none_wanted: bool,
) -> Result<Option<crate::secret::SecretBytes>> {
    let flag = format!("{which}-passphrase-file");
    let none_flag = format!("{which}-no-passphrase");
    match (a.get(&flag), none_wanted || a.has(&none_flag)) {
        (Some(_), true) => Err(WalletError::usage(format!(
            "choose one of --{flag} and --{none_flag}"
        ))),
        (None, true) => Ok(None),
        (None, false) => Err(WalletError::usage(format!(
            "--{flag} is required (a passphrase never goes on argv, and stdin is not \
             used here because two passphrases would be ambiguous)"
        ))),
        (Some(p), false) => {
            let bytes = std::fs::read(p)
                .map_err(|e| WalletError::io(format!("cannot read {p}: {e}")))?;
            let mut v = bytes;
            if v.last() == Some(&b'\n') {
                v.pop();
                if v.last() == Some(&b'\r') {
                    v.pop();
                }
            }
            if v.is_empty() {
                return Err(WalletError::usage(format!("passphrase file {p} is empty")));
            }
            Ok(Some(crate::secret::SecretBytes::from_vec(v)))
        }
    }
}

fn chain_line(network: Network) -> String {
    let id = network.chain_id();
    format!(
        "  network    {network}\n  chain_id   {} ({})",
        plaine_consensus::hex::encode(&id),
        String::from_utf8_lossy(&id)
    )
}

const TRANSFER_SPEC: Spec = Spec {
    values: &["in", "to", "amount", "fee", "nonce", "out", "passphrase-file"],
    switches: &["passphrase-stdin", "no-passphrase"],
};

fn cmd_transfer(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &TRANSFER_SPEC)?;
    let network = Network::Main;
    let kf = keyfile::read(Path::new(a.require("in")?))?;
    let to = a.require("to")?.to_string();
    let amount = amount::parse_mile_for("amount", a.require("amount")?)?;
    let fee = amount::parse_mile_for("fee", a.require("fee")?)?;
    let nonce = args::parse_u64("nonce", a.require("nonce")?)?;

    let pass = open_passphrase(&a, s, &kf)?;
    let key = api::open(Path::new(a.require("in")?), pass.as_ref())?;
    let tx = key.sign_transfer(network, &to, amount, fee, nonce)?;
    drop(key); // do not keep the decrypted seed alive past signing

    s.say("transfer: signed (type 0x01)");
    s.say(&format!("  txid       {}", plaine_consensus::hex::encode(&tx.txid)));
    s.say(&format!("  from       {}", tx.from));
    s.say(&format!("  to         {}", tx.to));
    s.say(&format!("  amount     {}", amount::describe(amount)));
    s.say(&format!("  fee        {}", amount::describe(fee)));
    s.say(&format!("  nonce      {nonce}"));
    s.say(&chain_line(network));
    s.say(&format!("  bytes      {}", tx.raw.len()));
    emit(s, &a, &tx.hex)?;
    fee_warning(s, fee);
    s.say("");
    s.say("The wallet has no network. Submit this with noded's RPC.");
    Ok(())
}

#[cfg(feature = "author-tools")]
const ANNOUNCE_SPEC: Spec = Spec {
    values: &[
        "in",
        "author-pubkey",
        "payload-file",
        "payload-hex",
        "encoding",
        "fee",
        "nonce",
        "out",
        "passphrase-file",
    ],
    switches: &["passphrase-stdin", "no-passphrase", "reuse-nonce"],
};

#[cfg(feature = "author-tools")]
fn cmd_announce(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &ANNOUNCE_SPEC)?;
    let network = Network::Main;
    let key_path = a.require("in")?.to_string();
    let kf = keyfile::read(Path::new(&key_path))?;

    let author = args::parse_pubkey("author-pubkey", a.require("author-pubkey")?)?;

    let payload = match (a.get("payload-file"), a.get("payload-hex")) {
        (Some(_), Some(_)) => {
            return Err(WalletError::usage(
                "choose one of --payload-file and --payload-hex",
            ))
        }
        (None, None) => {
            return Err(WalletError::usage(
                "--payload-file <path> or --payload-hex <hex> is required. The payload \
                 never goes on argv as text: argv reaches process listings and shell history.",
            ))
        }
        (Some(p), None) => std::fs::read(p)
            .map_err(|e| WalletError::io(format!("cannot read payload file {p}: {e}")))?,
        (None, Some(h)) => plaine_consensus::hex::decode(h)
            .map_err(|e| WalletError::usage(format!("--payload-hex: {e}")))?,
    };
    let encoding = {
        let v = a.require("encoding")?;
        let n = args::parse_u64("encoding", v)?;
        u8::try_from(n).map_err(|_| WalletError::usage("--encoding must fit in one byte"))?
    };
    let fee = amount::parse_mile_for("fee", a.require("fee")?)?;
    let nonce = args::parse_u64("nonce", a.require("nonce")?)?;

    let msg_hash =
        crypto::announcement_signing_message(network, &kf.pubkey, fee, nonce, encoding, &payload)
            .map_err(|e| WalletError::format(format!("announcement payload rejected: {e}")))?;
    let jpath = journal::path_for(Path::new(&key_path));
    let entries = journal::read(&jpath)?;
    let phash = journal::payload_hash(&payload);
    if let Some(prev) = journal::conflict(&entries, nonce, &msg_hash) {
        if !a.has("reuse-nonce") {
            let why = if prev.predates_message_recording() {
                "was already used by an announcement recorded before this build began \
                 journalling the signed message, so a retry cannot be told apart from a \
                 different fee or encoding"
            } else {
                "was already used for a different announcement"
            };
            return Err(WalletError::refused(format!(
                "nonce {nonce} {why} (txid {}, recorded at {}). Both would be valid and \
                 a miner picks which one lands. Use a fresh nonce, or --reuse-nonce if \
                 you are replacing the earlier message and it was never broadcast.",
                plaine_consensus::hex::encode(&prev.txid),
                prev.time
            )));
        }
        s.warn("warning: --reuse-nonce given; two different signed announcements now exist at this nonce");
    }

    let pass = open_passphrase(&a, s, &kf)?;
    let seed = kf.open(pass.as_ref())?;
    let tx =
        txbuild::build_announcement(network, &seed, &author, &payload, encoding, fee, nonce)?;
    drop(seed);

    let signed_msg = crypto::announcement_message_of(network, &tx)
        .map_err(|e| WalletError::crypto(format!("signing message: {e}")))?;
    if signed_msg != msg_hash {
        return Err(WalletError::crypto(
            "the signed announcement is not the message the nonce journal was checked \
             against; nothing was written",
        ));
    }

    let encoded = tx
        .encode()
        .map_err(|e| WalletError::crypto(format!("encode failed: {e}")))?;
    let txid = tx
        .txid()
        .map_err(|e| WalletError::crypto(format!("txid failed: {e}")))?;
    let hex = plaine_consensus::hex::encode(&encoded);

    s.say("announce: signed author announcement (type 0x02)");
    s.say(&format!("  txid       {}", plaine_consensus::hex::encode(&txid)));
    s.say(&format!("  from       {}", kf.address));
    s.say(&format!(
        "  author key {}",
        plaine_consensus::hex::encode(&author)
    ));
    s.say(&format!("  fee        {}", amount::describe(fee)));
    s.say(&format!("  nonce      {nonce}"));
    s.say(&format!("  encoding   0x{encoding:02x}"));
    s.say(&format!("  payload    {} bytes", payload.len()));
    s.say(&chain_line(network));
    print_payload(s, &payload);
    s.say(&format!("  bytes      {}", encoded.len()));
    emit(s, &a, &hex)?;
    fee_warning(s, fee);

    journal::append(
        &jpath,
        &journal::Entry {
            time: crate::now_secs(),
            nonce,
            txid,
            payload_hash: phash,
            msg_hash: Some(signed_msg),
        },
    )?;
    s.say(&format!("  journal    appended to {}", jpath.display()));
    s.say("");
    s.say("A block carrying an announcement from a non-matching key is invalid,");
    s.say("so a miner including a wrong-key announcement loses the whole block.");
    s.say("The key above was checked against --author-pubkey before anything was signed.");
    Ok(())
}

fn print_payload(s: &mut Streams, payload: &[u8]) {
    let p = sanitize::preview(payload);
    s.say(&format!("  valid_utf8 {}", p.valid_utf8));

    s.say(&format!(
        "  payload_hex {}",
        plaine_consensus::hex::encode(payload)
    ));
    if p.valid_utf8 {
        s.say(&format!("  preview    {}", p.text));
        if p.removed > 0 {
            s.warn(&format!(
                "warning: the preview dropped {} invisible or control character(s) present \
                 in the payload",
                p.removed
            ));
        }
        s.say("  (preview is best-effort: controls, bidi and zero-width are stripped;");
        s.say("   NFC normalisation is not applied - no Unicode tables in this crate)");
    }
}

fn fee_warning(s: &mut Streams, fee: u128) {
    if txbuild::fee_is_at_the_floor(fee) {
        s.warn(&format!(
            "warning: fee {fee} mile is at the consensus floor ({FEE_FLOOR_MILE} mile). \
             Block selection is by fee priority, so inclusion is not \
             time-bounded. For an emergency announcement this is not good enough."
        ));
    }
}

fn emit(s: &mut Streams, a: &Parsed, hex: &str) -> Result<()> {
    match a.get("out") {
        Some(p) => {
            if Path::new(p).exists() {
                return Err(WalletError::refused(format!(
                    "{p} already exists; refusing to overwrite"
                )));
            }
            std::fs::write(p, format!("{hex}\n"))
                .map_err(|e| WalletError::io(format!("cannot write {p}: {e}")))?;
            s.say(&format!("  wrote      {p}"));
        }
        None => {
            s.say(&format!("  hex        {hex}"));
        }
    }
    Ok(())
}

const DECODE_SPEC: Spec = Spec {
    values: &["in", "hex", "author-pubkey"],
    switches: &[],
};

fn cmd_decode(argv: &[String], s: &mut Streams) -> Result<()> {
    let a = args::parse(argv, &DECODE_SPEC)?;

    let network = Network::Main;
    let hex = match (a.get("in"), a.get("hex")) {
        (Some(_), Some(_)) => return Err(WalletError::usage("choose one of --in and --hex")),
        (None, None) => return Err(WalletError::usage("--in <file> or --hex <hex> is required")),
        (Some(p), None) => std::fs::read_to_string(p)
            .map_err(|e| WalletError::io(format!("cannot read {p}: {e}")))?
            .trim()
            .to_string(),
        (None, Some(h)) => h.to_string(),
    };
    let bytes = plaine_consensus::hex::decode(&hex)
        .map_err(|e| WalletError::format(format!("input is not hex: {e}")))?;
    if bytes.is_empty() {
        return Err(WalletError::format("input is empty"));
    }
    match bytes[0] {
        0x01 => {
            let tx = TransferTx::decode(&bytes)
                .map_err(|e| WalletError::format(format!("not a valid transfer: {e}")))?;
            s.say("decode: transfer (type 0x01)");
            s.say(&format!("  txid       {}", plaine_consensus::hex::encode(&tx.txid())));
            s.say(&format!(
                "  from       {}",
                crypto::address_from_pubkey(&tx.from_pub)
            ));
            s.say(&format!("  from_pub   {}", plaine_consensus::hex::encode(&tx.from_pub)));
            s.say(&format!("  to         {}", crypto::encode_address(&tx.to)));
            s.say(&format!("  amount     {}", amount::describe(tx.amount)));
            s.say(&format!("  fee        {}", amount::describe(tx.fee)));
            s.say(&format!("  nonce      {}", tx.nonce));
            s.say(&chain_line(network));
            match crypto::verify_transfer_signature(network, &tx) {
                Ok(()) => s.say(&format!("  signature  VALID on {network}")),
                Err(e) => {
                    s.say(&format!("  signature  INVALID on {network}: {e}"));
                    return Err(WalletError::crypto("signature verification failed"));
                }
            }
        }
        0x02 => {
            let tx = AnnouncementTx::decode(&bytes)
                .map_err(|e| WalletError::format(format!("not a valid announcement: {e}")))?;
            s.say("decode: author announcement (type 0x02)");
            s.say(&format!(
                "  txid       {}",
                plaine_consensus::hex::encode(&tx.txid().map_err(|e| WalletError::format(
                    format!("txid: {e}")
                ))?)
            ));
            s.say(&format!("  from_pub   {}", plaine_consensus::hex::encode(&tx.from_pub)));
            s.say(&format!(
                "  from       {}",
                crypto::address_from_pubkey(&tx.from_pub)
            ));
            s.say(&format!("  fee        {}", amount::describe(tx.fee)));
            s.say(&format!("  nonce      {}", tx.nonce));
            s.say(&format!("  encoding   0x{:02x}", tx.encoding));
            s.say(&format!("  payload    {} bytes", tx.payload.len()));
            print_payload(s, &tx.payload);
            s.say(&chain_line(network));
            match crypto::verify_announcement_signature(network, &tx) {
                Ok(()) => s.say(&format!("  signature  VALID on {network}")),
                Err(e) => {
                    s.say(&format!("  signature  INVALID on {network}: {e}"));
                    return Err(WalletError::crypto("signature verification failed"));
                }
            }
            match a.get("author-pubkey") {
                Some(v) => {
                    let author = args::parse_pubkey("author-pubkey", v)?;
                    match plaine_consensus::tx::check_announcement_stateless(
                        network, &tx, &author,
                    ) {
                        Ok(()) => s.say("  rule 1     from_pub == --author-pubkey"),
                        Err(e) => {
                            s.say(&format!("  rule 1     REJECTED: {e}"));
                            return Err(WalletError::crypto(
                                "check_announcement_stateless rejected this announcement",
                            ));
                        }
                    }
                }
                None => s.warn(
                    "note: pass --author-pubkey to also check SPEC 4.1 rule 1. Without it \
                     only the signature was verified, and a signature proves nothing about \
                     whose key it is supposed to be.",
                ),
            }
        }
        0x00 => {
            return Err(WalletError::format(
                "this is a coinbase (type 0x00); coinbases are produced by miners, not \
                 wallets, so this tool does not decode them.",
            ))
        }
        other => {
            return Err(WalletError::format(format!(
                "unknown transaction type byte 0x{other:02x} (0x03..=0xFF are reserved \
                 and consensus rejects them)"
            )))
        }
    }
    Ok(())
}

pub fn seal_and_write(
    path: &Path,
    role: Role,
    seed: &Secret32,
    pass: Option<&crate::secret::SecretBytes>,
    iters: u64,
) -> Result<KeyFile> {
    let kf = KeyFile::seal(role, crate::now_secs(), seed, pass, iters)?;
    keyfile::create_verified(path, &kf, pass, &kf.pubkey)?;
    Ok(kf)
}

pub use genesis::FROZEN_NOTE_RULE;

#[cfg(test)]
mod drift_guards {
    use super::*;
    use crate::ui::{
        PASSPHRASE_SWITCH_FLAGS, PASSPHRASE_VALUE_FLAGS, SEED_SWITCH_FLAGS, SEED_VALUE_FLAGS,
    };

    #[test]
    fn key_specs_offer_all_passphrase_sources() {
        #[cfg_attr(not(feature = "author-tools"), allow(unused_mut))]
        let mut specs: Vec<(&str, &Spec)> = vec![
            ("new/import", &CREATE_SPEC),
            ("verify", &OPEN_SPEC),
            ("backup", &BACKUP_SPEC),
            ("transfer", &TRANSFER_SPEC),
        ];

        #[cfg(feature = "author-tools")]
        specs.push(("announce", &ANNOUNCE_SPEC));
        for (name, spec) in specs {
            for f in PASSPHRASE_VALUE_FLAGS {
                assert!(spec.values.contains(f), "{name} must take --{f}");
            }
            for f in PASSPHRASE_SWITCH_FLAGS {
                assert!(spec.switches.contains(f), "{name} must take --{f}");
            }
        }
        for f in SEED_VALUE_FLAGS {
            assert!(CREATE_SPEC.values.contains(f), "new/import must take --{f}");
        }
        for f in SEED_SWITCH_FLAGS {
            assert!(CREATE_SPEC.switches.contains(f), "new/import must take --{f}");
        }
    }

    #[test]
    fn amounts_are_parsed_with_a_named_flag() {

        let flagless = String::from("parse_mile") + "(";
        let flagged = String::from("parse_mile_for") + "(";
        let src = include_str!("wallet_cli.rs");
        let mut hits = 0usize;
        for (i, line) in src.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains(flagged.as_str()) {
                hits += 1;
                continue;
            }
            assert!(
                !code.contains(flagless.as_str()),
                "wallet_cli.rs:{} parses an amount without naming its flag. Call amount::parse_mile_for with the flag name, so the refusal names the field the operator has to edit:\n  {line}",
                i + 1
            );
        }

        assert_eq!(hits, 3, "transfer --amount, transfer --fee, announce --fee");
    }
}
