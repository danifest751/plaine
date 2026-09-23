# Plaine desktop wallet

A desktop wallet for Plaine. The keys are handled by `plaine-wallet`'s own code
(`plaine_wallet::api`); the node is reached over its local JSON-RPC, and nothing else
goes over the network. It is a separate Cargo workspace, so neither the node nor the
command-line wallet links a GUI toolkit (`tests/separation.rs` checks).

## Build and run

```
cargo build --release --manifest-path wallet-gui/Cargo.toml
wallet-gui/target/release/plaine-wallet-gui
```

On Windows with the GNU toolchain, put GNU `dlltool` first on `PATH` as `FORK.md`
describes (`scripts/check.sh` sets up `target/.tools`).

It needs a running `plaine-noded`; the default address is `127.0.0.1:9257`, changeable
in Settings together with an RPC token.

## What it does

- **Start:** open a key file, create a new key, or restore one from its 68-character
  backup string. New keys are sealed with argon2id (`kdf: argon2id-v1`), with a
  passphrase of at least 12 characters; *Suggest a passphrase* makes a random one. A new
  key's backup string is shown once, until you confirm you wrote it down.
- **Home:** the spendable balance in large type, maturing and total balance, pending
  sends; your address with a copy button and a QR code; the node's state in the status
  bar. An unencrypted key file (`kdf: none`) is flagged.
- **Send:** recipient checked as bech32m, amount in PLNE, fee Low/Normal/High from the
  node's `fee_suggest`, never below the relay floor; the nonce comes from the node's
  `pendingNonce`. A confirmation screen shows the total before anything is signed.
- **History:** pending transfers first, then confirmed ones, newest first, paged.
- **Mining:** starts `plaine-miner` (by default the one beside the wallet) paying to this
  wallet's address, through the node's stratum server (same host, port 9258) or any
  other. *Background* mines on half the cores at idle priority, *Maximum* on all of them.
  The tab shows hash rate, accepted and rejected shares, blocks found and height, read
  from the miner's `--status-format json` lines. Mining needs no key, so it goes on while
  the wallet is locked; closing the wallet stops it.
- **Settings:** node address and token, the auto-lock delay, writing a copy of the key
  under a new passphrase (argon2id), and showing the backup string after the passphrase
  is entered again. A copied backup string is wiped from the clipboard after 60 seconds.

The key is dropped when you press *Lock* and after the auto-lock delay (10 minutes by
default).

## With an upstream node

Consensus is the same, so balance and sending work with any `plaine-noded`. History
needs `account_getHistory`, which only this fork's node has, and only with
`addrindex = true` (and `prune = false` for the whole history). With a node that cannot
answer it, the History tab lists what this wallet sent, from a log beside the key file
(`<key file>.sent`), each marked confirmed once the account nonce has moved past it, and
says that incoming transfers need an indexing node.

## `plaine-wallet-cli`

The same command line as `plaine-wallet`, built here so that it can seal and open
argon2id key files (`--kdf argon2id`).

```
wallet-gui/target/release/plaine-wallet-cli help
```

## Tests

```
cargo test --release --manifest-path wallet-gui/Cargo.toml
```

The screens are driven headless with `egui_kittest` against a node in memory, including
one shaped like upstream's and one that is down; the HTTP client is tested against a
real socket; argon2id against the reference implementation's vector.

`tests/e2e.rs` runs the whole wallet against a real `plaine-noded` and `plaine-miner` on
a fresh chain: two keys created on the first-run screen, one mined past coinbase
maturity, a transfer sent on the send screen and seen by both. It takes a few minutes, so
it is ignored by default:

```
cargo test --release --manifest-path wallet-gui/Cargo.toml --test e2e -- --ignored
```

`scripts/check.sh --e2e` runs it, after building the node and the miner it needs.
