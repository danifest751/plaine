# Changelog

Changes in this fork relative to [upstream](https://github.com/noaltitude/plaine) at
`189598d`. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Unreleased

### Added

- Wallet: key files can be sealed under `kdf: argon2id-v1` (64 MiB, one lane, three
  passes by default), a memory-hard KDF, instead of `blake3-iter-v1`, which is not.
  `--kdf argon2id` on `new`, `import` and `passphrase`. The algorithm is installed by the
  program: `plaine-wallet-cli` and the desktop wallet, both in `wallet-gui/`, have it;
  the plain `plaine-wallet` keeps upstream's dependency policy and refuses such files by
  name. Every existing key file opens as before; `kdf: none` and `blake3-iter-v1` files
  written by upstream's wallet are kept as test fixtures and opened on every run.
- Wallet: `plaine_wallet::api`, the wallet as a library for the desktop wallet: create,
  import and open a key, sign a transfer, print the backup string, change the
  passphrase, parse amounts and check addresses, with nothing written to a terminal.
  Advice the CLI prints comes back as `Notice` values. The CLI now runs on it; its output
  is unchanged. One difference: `transfer` now checks that the decrypted seed reproduces
  the key file's public key before signing, as `verify` always did.
- `docs/rpc.md`: a reference for all 19 RPC methods: transport rules, parameters, result
  fields, errors and examples, checked against a running node. `lib/rpc/tests/reference_doc.rs`
  fails when the method list and the reference drift apart.
- Node: opt-in address index, `[node] addrindex = true`, and the RPC method
  `account_getHistory(address, limit?, cursor?)`: an address's confirmed transactions,
  newest first, each with kind, direction, amount, fee and counterparty, paged by cursor.
  Rows survive reorgs and are checked against the canonical body on read, as `txindex`
  hits are. Documented in `docs/rpc.md`.
- Node: `tx_get(txid, address)` finds a confirmed transaction on a node without `txindex`,
  through the address index of an address the transaction touches.
- Node tests: `transfer_roundtrip.rs` follows a transfer signed by `plaine-wallet` from
  `tx_sendRaw` through the mempool into both parties' histories, and finds it again after
  a restart. It mines past coinbase maturity, so it runs under `scripts/check.sh --e2e`.
  The test miner now leaves two cores free.
- Storage: a block body that cannot be parsed is refused before anything is written;
  failing half-way through a block used to leave the committer unable to shut down.
- Miner: `--batch N` (and `"batch"` in the config file) sets how many nonces one W^X seal
  covers. By default it is sized so the batch's pads fit half the thread's L2 share;
  measured 27.12 kH/s at 4 against 24.88 kH/s at the old fixed 32 on a Ryzen 7 8745HS.
- Miner: workers are pinned one per physical core before any SMT sibling by default;
  `--no-pin` (or `"no-pin": true`) restores OS placement. Measured 25.0 kH/s pinned
  against 23.3–23.7 kH/s unpinned.
- `scripts/check.sh`: every check a commit must pass, in one command. On windows-gnu it
  takes GNU `dlltool` and `as` from MSYS2 into `target/.tools`.
- `.gitattributes`: shell scripts stay LF on Windows checkouts.
- `FORK.md`, `CONTRIBUTING.md`, this changelog, and the roadmap in `docs/ROADMAP.md`.

### Changed

- Node: **the stratum server listens on `127.0.0.1:9258` by default**, not `0.0.0.0:9258`.
  A node started with no config no longer opens its mining server to every network it is
  on. Miners on other machines, or a pool front-end, need `[stratum] listen =
  "0.0.0.0:9258"`; a node on loopback says so in its log at start-up. Configs that already
  set `listen` are unaffected.

### Fixed

- Node: `fee_suggest` sampled no blocks and returned the relay floor as every percentile.
  It now takes nearest-rank percentiles of the transfer fees in the last 240 blocks (the
  window SPEC §14 names), never below the relay floor, computed once per tip.
- Node: after a reorg, `author_getNotes` lost the announcements of the branch it had just
  applied: their notes were added and then removed by the rollback meant for the orphaned
  blocks. The rollback now runs first.
- Node: `chain_getBlockByHash` with the hash of a side-branch block returned the best
  chain's body at that height under the side header. It now answers not found; the header
  is still served by `chain_getHeaderByHash`.
- Node: with `txindex` on, `tx_get` for a transaction in a pruned block said the index began
  at that height. It now says the block's body was pruned.
- Miner: `mining.submit` sent a reformatted job id (`"2341"` came back as `"00002341"`),
  so pools that do not pad ids refused every share. It now echoes the server's spelling.
- Miner: a `result` that is a JSON object (`{"status":"OK"}`) made the whole line fail to
  parse; the share was counted as neither accepted nor rejected and its id leaked. Such
  results are parsed, and a submit counts as refused only on an error object or `false`.
- Miner: more than 256 workers repeated each other's nonces and got the IP banned for
  duplicate shares. The worker count is capped at the number of nonce lanes.
- Miner: mining and `--bench` now refuse to start when the JIT does not reproduce the
  frozen `SELF_CHECK` vectors.
- Miner: `--bench` blamed a missing `SeLockMemoryPrivilege` for every huge-page failure,
  including under `--no-huge-pages`. It now prints the allocator's own reason.
- Node tests: nodes the integration harness meant to isolate dialled the embedded mainnet
  seeds, downloaded the real chain and had it reorg the test chain away — the cause of
  intermittent `ibd.rs` failures, and load on the network's seed nodes on every run. Test
  configs now set `use_embedded_seeds = false`; `isolation.rs` checks it from the node log.
- Stratum tests: `stalled_write_closes_at_deadline` failed two runs in three on Windows
  under parallel load, because the OS auto-tuned the loopback buffers the test relied on
  staying full. It now fixes the buffer sizes and waits until the socket is truly stalled.
- `.gitignore` did not cover `miner.key`, the file the upstream README's quick start
  writes the key to, nor the passphrase files the README documents.
