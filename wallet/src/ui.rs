use crate::error::{Result, WalletError};
use crate::secret::SecretBytes;
use std::io::{Read, Write};

pub struct Streams<'a> {
    stdin: &'a mut dyn Read,
    cache: Option<Vec<u8>>,
    consumed_lines: usize,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

impl<'a> Streams<'a> {
    pub fn new(stdin: &'a mut dyn Read, out: &'a mut dyn Write, err: &'a mut dyn Write) -> Self {
        Streams {
            stdin,
            cache: None,
            consumed_lines: 0,
            out,
            err,
        }
    }

    pub fn stdin_bytes(&mut self) -> Result<&[u8]> {
        if self.cache.is_none() {
            let mut buf = Vec::new();
            self.stdin
                .read_to_end(&mut buf)
                .map_err(|e| WalletError::io(format!("cannot read stdin: {e}")))?;
            self.cache = Some(buf);
        }
        Ok(self.cache.as_deref().expect("just filled"))
    }

    pub fn next_stdin_line(&mut self) -> Result<Vec<u8>> {
        let want = self.consumed_lines;
        // NOTE: this re-splits the whole buffer on every call. we read one or
        // two lines total (a passphrase, at most twice), so it isn't worth
        // caching the split.
        let bytes = self.stdin_bytes()?.to_vec();
        let mut lines: Vec<&[u8]> = Vec::new();
        let mut start = 0usize;
        for i in 0..bytes.len() {
            if bytes[i] == b'\n' {
                lines.push(&bytes[start..i]);
                start = i + 1;
            }
        }
        if start < bytes.len() {
            lines.push(&bytes[start..]);
        }
        let line = lines.get(want).ok_or_else(|| {
            WalletError::io(format!(
                "stdin ended after {want} line(s); this command needs another one"
            ))
        })?;
        self.consumed_lines += 1;
        let mut v = line.to_vec();
        if v.last() == Some(&b'\r') {
            v.pop();
        }
        Ok(v)
    }

    pub fn say(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
    }

    pub fn warn(&mut self, line: &str) {
        let _ = writeln!(self.err, "{line}");
    }

    pub fn warn_block(&mut self, lines: &[&str]) {
        let _ = writeln!(self.err, "{}", "!".repeat(72));
        for l in lines {
            let _ = writeln!(self.err, "! {l}");
        }
        let _ = writeln!(self.err, "{}", "!".repeat(72));
    }
}

pub const PASSPHRASE_VALUE_FLAGS: &[&str] = &["passphrase-file"];
pub const PASSPHRASE_SWITCH_FLAGS: &[&str] = &["passphrase-stdin", "no-passphrase"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassMode {
    Open,
    Create,
}

pub fn passphrase(
    args: &crate::args::Parsed,
    s: &mut Streams,
    mode: PassMode,
) -> Result<Option<SecretBytes>> {
    let file = args.get("passphrase-file");
    let from_stdin = args.has("passphrase-stdin");
    let none = args.has("no-passphrase");
    let chosen = [file.is_some(), from_stdin, none]
        .iter()
        .filter(|b| **b)
        .count();
    if chosen > 1 {
        return Err(WalletError::usage(
            "choose exactly one of --passphrase-file, --passphrase-stdin, --no-passphrase",
        ));
    }
    if none {
        return Ok(None);
    }
    if let Some(path) = file {
        let bytes = std::fs::read(path)
            .map_err(|e| WalletError::io(format!("cannot read passphrase file {path}: {e}")))?;
        let trimmed = strip_one_trailing_newline(bytes);
        if trimmed.is_empty() {
            return Err(WalletError::usage(format!(
                "passphrase file {path} is empty"
            )));
        }
        return Ok(Some(SecretBytes::from_vec(trimmed)));
    }
    if from_stdin {
        let first = s.next_stdin_line()?;
        if first.is_empty() {
            return Err(WalletError::usage("the passphrase read from stdin is empty"));
        }
        if mode == PassMode::Create {
            // read twice and compare: there is no echo-off prompt without a
            // dependency or unsafe FFI, so the passphrase is piped in
            let second = s.next_stdin_line()?;
            if !crate::secret::ct_eq(&first, &second) {
                return Err(WalletError::usage(
                    "the two passphrases do not match; nothing was created",
                ));
            }
        }
        return Ok(Some(SecretBytes::from_vec(first)));
    }
    Err(WalletError::usage(
        "no passphrase source given; use --passphrase-file <path>, --passphrase-stdin, \
         or --no-passphrase",
    ))
}

fn strip_one_trailing_newline(mut v: Vec<u8>) -> Vec<u8> {
    if v.last() == Some(&b'\n') {
        v.pop();
        if v.last() == Some(&b'\r') {
            v.pop();
        }
    }
    v
}

pub const SEED_VALUE_FLAGS: &[&str] = &["seed-file"];
pub const SEED_SWITCH_FLAGS: &[&str] = &["seed-stdin"];

pub fn seed(
    args: &crate::args::Parsed,
    s: &mut Streams,
    restoring: bool,
) -> Result<crate::secret::Secret32> {
    let file = args.get("seed-file");
    let from_stdin = args.has("seed-stdin");
    let material = match (file, from_stdin) {
        (Some(_), true) => {
            return Err(WalletError::usage(
                "choose one of --seed-file and --seed-stdin",
            ))
        }
        (None, false) => {
            return Err(WalletError::usage(
                "no seed source given. `new` mints a fresh key from the OS CSPRNG when you \
                 omit these flags; to restore or to supply your own entropy, pass the \
                 material with --seed-stdin or --seed-file",
            ))
        }
        (Some(path), false) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| WalletError::io(format!("cannot read seed file {path}: {e}")))?;
            text.trim().to_string()
        }
        (None, true) => {
            if args.has("passphrase-stdin") {
                return Err(WalletError::usage(
                    "--seed-stdin and --passphrase-stdin both read stdin; supply the \
                     passphrase with --passphrase-file instead",
                ));
            }
            let bytes = s.stdin_bytes()?.to_vec();
            let text = String::from_utf8(bytes)
                .map_err(|_| WalletError::format("stdin is not text; expected 64 hex digits"))?;
            text.trim().to_string()
        }
    };
    decode_and_check(&material, s, restoring)
}

fn decode_and_check(
    text: &str,
    s: &mut Streams,
    restoring: bool,
) -> Result<crate::secret::Secret32> {
    let (seed, notice) = crate::api::decode_seed_text(text, restoring)?;
    if let Some(n) = notice {
        for line in n.lines() {
            s.warn(&line);
        }
    }
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{parse, Spec};

    const SPEC: Spec = Spec {
        values: &["passphrase-file", "seed-file"],
        switches: &["passphrase-stdin", "no-passphrase", "seed-stdin"],
    };

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn with(input: &[u8], f: impl FnOnce(&mut Streams)) {
        let mut r = input;
        let mut o = Vec::new();
        let mut e = Vec::new();
        let mut s = Streams::new(&mut r, &mut o, &mut e);
        f(&mut s);
    }

    #[test]
    fn stdin_lines_are_handed_out_in_order() {
        with(b"one\ntwo\nthree", |s| {
            assert_eq!(s.next_stdin_line().unwrap(), b"one");
            assert_eq!(s.next_stdin_line().unwrap(), b"two");
            assert_eq!(s.next_stdin_line().unwrap(), b"three");
            assert!(s.next_stdin_line().is_err());
        });
    }

    #[test]
    fn crlf_is_stripped() {
        with(b"one\r\ntwo\r\n", |s| {
            assert_eq!(s.next_stdin_line().unwrap(), b"one");
            assert_eq!(s.next_stdin_line().unwrap(), b"two");
        });
    }

    #[test]
    fn creation_reads_passphrase_twice() {
        let args = parse(&a(&["--passphrase-stdin"]), &SPEC).unwrap();
        with(b"secret\nsecret\n", |s| {
            let p = passphrase(&args, s, PassMode::Create).unwrap().unwrap();
            assert_eq!(p.expose(), b"secret");
        });
        with(b"secret\nsecrer\n", |s| {
            let err = passphrase(&args, s, PassMode::Create).unwrap_err();
            assert!(err.to_string().contains("do not match"));
            assert!(err.to_string().contains("nothing was created"));
        });

        with(b"secret\n", |s| {
            let p = passphrase(&args, s, PassMode::Open).unwrap().unwrap();
            assert_eq!(p.expose(), b"secret");
        });
    }

    #[test]
    fn passphrase_sources_are_mutually_exclusive() {
        let args = parse(&a(&["--passphrase-stdin", "--no-passphrase"]), &SPEC).unwrap();
        with(b"", |s| {
            assert!(passphrase(&args, s, PassMode::Open).is_err());
        });
        let none = parse(&a(&[]), &SPEC).unwrap();
        with(b"", |s| {
            let err = passphrase(&none, s, PassMode::Open).unwrap_err();
            assert!(err.to_string().contains("no passphrase source"));
        });
    }

    #[test]
    fn no_passphrase_means_none() {
        let args = parse(&a(&["--no-passphrase"]), &SPEC).unwrap();
        with(b"", |s| {
            assert!(passphrase(&args, s, PassMode::Create).unwrap().is_none());
        });
    }

    #[test]
    fn seed_needs_source_and_real_entropy() {
        let none = parse(&a(&[]), &SPEC).unwrap();
        with(b"", |s| {
            let err = seed(&none, s, false).unwrap_err();
            assert!(err.to_string().contains("no seed source given"));
        });

        let args = parse(&a(&["--seed-stdin"]), &SPEC).unwrap();
        let good = plaine_consensus::hex::encode(&plaine_consensus::blake3::hash(b"ui seed"));
        with(good.as_bytes(), |s| {
            assert!(seed(&args, s, false).is_ok());
        });
        with(&b"00".repeat(32), |s| {
            assert!(seed(&args, s, false).is_err(), "all zero must be refused");
        });
        with(b"deadbeef", |s| {
            assert!(seed(&args, s, false).is_err(), "wrong length");
        });
    }

    #[test]
    fn seed_stdin_and_passphrase_stdin_conflict() {
        let args = parse(&a(&["--seed-stdin", "--passphrase-stdin"]), &SPEC).unwrap();
        with(b"x", |s| {
            let err = seed(&args, s, false).unwrap_err();
            assert!(err.to_string().contains("both read stdin"));
        });
    }

    #[test]
    fn restoring_demands_checksummed_form() {
        let args = parse(&a(&["--seed-stdin"]), &SPEC).unwrap();
        let raw = crate::secret::Secret32::from_bytes(plaine_consensus::blake3::hash(b"ui restore"));
        let bare = crate::sechex::encode(raw.expose());
        let checked = crate::sechex::encode_backup(&raw);

        with(bare.as_bytes(), |s| {
            assert!(seed(&args, s, false).is_ok());
        });
        with(checked.as_bytes(), |s| {
            let got = seed(&args, s, false).unwrap();
            assert_eq!(got.expose(), raw.expose());
        });

        with(checked.as_bytes(), |s| {
            let got = seed(&args, s, true).unwrap();
            assert_eq!(got.expose(), raw.expose(), "an exact restore is exact");
        });

        with(bare.as_bytes(), |s| {
            let err = seed(&args, s, true).unwrap_err();
            assert_eq!(err.kind(), "refused");
            assert!(err.to_string().contains("new"), "{err}");
            assert!(!err.to_string().contains(&bare), "the refusal must not echo it");
        });

        let mut v: Vec<u8> = checked.as_bytes().to_vec();
        v[3] = if v[3] == b'a' { b'b' } else { b'a' };
        let typo = String::from_utf8(v).unwrap();
        for restoring in [false, true] {
            with(typo.as_bytes(), |s| {
                let err = seed(&args, s, restoring).unwrap_err();
                assert!(
                    err.to_string().contains("checksum") || err.to_string().contains("hexadecimal"),
                    "{err}"
                );
                assert!(!err.to_string().contains(&typo), "the refusal must not echo it");
            });
        }
    }
}
