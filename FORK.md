# This fork

`danifest751/plaine` is a fork of [noaltitude/plaine](https://github.com/noaltitude/plaine).
It tracks upstream and adds a desktop wallet, the node and wallet work that wallet needs,
and fixes to the reference miner. Consensus is not changed here: a block or transaction
valid for upstream is valid for the fork and the other way round.

Work stays in this repository. Apart from three PRs opened early on, nothing is proposed
upstream; instead, every defect the fork found in upstream's code is written up below with
its symptom, cause, fix and the test that catches it, so anyone can take it from here.

The plan and its status are in [docs/ROADMAP.md](docs/ROADMAP.md); every change is listed
in [CHANGELOG.md](CHANGELOG.md).

## What the fork adds

**Node and RPC**

- An opt-in address index, `[node] addrindex = true`, and the RPC method
  `account_getHistory(address, limit?, cursor?)`: an address's confirmed transactions,
  newest first, each with kind, direction, amount, fee and counterparty.
- `tx_get(txid, address)`: with the address index, a confirmed transaction is found on a
  node without `txindex`, through an address it touches. A wallet follows its own
  transfers this way.
- `fee_suggest` computes its percentiles from the transfers of the last 240 blocks (see
  the defects below).
- [docs/rpc.md](docs/rpc.md): a reference for every RPC method, checked against a running
  node, and `lib/rpc/tests/reference_doc.rs`, which fails when the two drift apart.

**Wallet**

- `plaine_wallet::api`: the wallet as a library, which the CLI now runs on and the
  desktop wallet will.
- A memory-hard key file KDF, `kdf: argon2id-v1`: 64 MiB and one lane per guess, three
  passes by default, stored in the same key file format. The wallet crate may depend on
  nothing but plaine-consensus, ed25519-dalek and getrandom (upstream pins that in
  `wallet/tests/end_to_end.rs`), so it knows the format and takes argon2id as a function
  a program installs at start-up. `wallet-gui/` links the `argon2` crate and installs it,
  in the desktop wallet and in `plaine-wallet-cli`, the same command line with
  `--kdf argon2id` available. The node does not compile it.
- Key files made before keep opening, with any build: `kdf: none` and
  `blake3-iter-v1`. `plaine-wallet-cli passphrase --kdf argon2id` moves one to
  argon2id, same address, original untouched. The CLI's default for new
  files stays `blake3-iter-v1`, so what it writes still opens in upstream's wallet;
  upstream's wallet cannot open an `argon2id-v1` file and says so.

**Miner**

- The W^X batch is sized to the machine's L2 (`--batch N` to override), and workers are
  pinned one per core before any SMT sibling (`--no-pin` to opt out), fastest core class
  first.
- Android: the processor topology is read as on Linux, and big.LITTLE cores are told
  apart by `cpu_capacity`, so a phone's big cores are used first.
- `--status-format json`: one JSON object per line on stdout (status, share, block,
  summary) for a program to read; the desktop wallet's Mining tab uses it.

**Tests and tooling**

- `node/tests/transfer_roundtrip.rs`: a transfer signed by `plaine-wallet` followed from
  `tx_sendRaw` through the mempool into both parties' histories, and found again after a
  restart. Upstream's end-to-end tests mine a few blocks; none spends a mined reward.
- `scripts/check.sh`: every check a commit must pass, in one command, instead of CI.
- `wallet-gui/`: the desktop wallet, in its own workspace; see `wallet-gui/README.md`.

## What the fork changes on purpose

- **The stratum server listens on `127.0.0.1:9258` by default**, not `0.0.0.0:9258`. The
  common setup mines on the node's own machine, and a node started with no config should
  not open a mining server to every network it is on. Miners on other machines, or a pool
  front-end, need `[stratum] listen = "0.0.0.0:9258"`; a node on loopback says so in its
  log. **Anyone upgrading whose rigs connect from other machines must add that line.**
- The node test harness runs the test miner on all but two cores, not all but one.

## Upstream defects fixed here

Each entry names the test that fails without the fix.

### Node

**`author_getNotes` loses announcements after a reorg.**
*Symptom:* after any reorg, the author announcements carried by the newly applied blocks
are missing from `author_getNotes`, until the node sees them again in later blocks, which
it never does. An operator misses announcements such as upgrade notices.
*Cause:* `commit_reorg` and `commit_deep_reorg` in `node/src/wire/store.rs` added the notes
of the applied blocks and then called `rollback_above(fork_height)`, which drops every note
above the fork, the new ones included.
*Fix:* `NoteIndex::on_reorg` rolls back first and applies second; both paths use it.
*Test:* `a_reorg_keeps_the_notes_of_the_branch_it_applies` in `node/src/wire/rpcview.rs`.

**`chain_getBlockByHash` answers a side-branch hash with another block's body.**
*Symptom:* asked for a block on a side branch, the node returns that block's header
together with the body of the best-chain block at the same height: transactions that are
not in the requested block.
*Cause:* `header_by_hash` also finds side headers; `block_record` then reads the body by
height, and bodies are stored for the best chain only.
*Fix:* `block_by_hash` answers not found unless the hash is the best chain's at its height.
The side header is still served by `chain_getHeaderByHash`.
*Test:* `a_side_branch_hash_is_not_served_the_best_chain_body` in `node/src/wire/rpcview.rs`.

**`tx_get` blames the txid index for a pruned block.**
*Symptom:* with `txindex = true` on a pruned node, a transaction in a block whose body is
gone gets "this node's txid index begins at height N", which is false: the index is
complete, the body was pruned.
*Cause:* the view reported the missing body as `NotIndexed { indexed_from: Some(height) }`.
*Fix:* a `TxLookup::Pruned { height }` case, and a message that names pruning and what
would answer instead.
*Test:* `tx_get_in_a_pruned_block_blames_pruning_not_the_index` in `lib/rpc/src/methods.rs`.

**`fee_suggest` samples nothing.**
*Symptom:* `blocksSampled` is always 0 and every percentile equals the relay floor, while
SPEC §14 describes "fee percentiles over the last 240 blocks". A wallet has nothing to base
a fee on.
*Cause:* the node's `MempoolView::fee_suggest` returned the floor as a placeholder.
*Fix:* nearest-rank p10/p50/p90 of the transfer fees in the last 240 blocks, never below
the relay floor, cached per tip; the sample stops at the first pruned body.
*Tests:* `fee_percentiles_are_nearest_rank_and_never_below_the_floor` and
`fee_sampling_reads_transfers_back_from_the_tip_and_stops_at_a_gap` in
`node/src/wire/rpcview.rs`; the end-to-end check in `transfer_roundtrip.rs`.

### Miner (also sent upstream as #1 and #2 before the fork stopped proposing)

**Pools refuse every share when the job id is not padded.** `mining.submit` sent the job
id reformatted to eight hex digits (`"2341"` came back as `"00002341"`); pools that do not
pad ids, rplant.xyz among them, refused every share. The id is now echoed exactly as the
server sent it.

**An object `result` is not read as an accepted share.** A response such as
`{"result":{"status":"OK"}}` failed to parse as a whole line; the share was counted as
neither accepted nor refused and its request id leaked. Such results are parsed, and a
submit counts as refused only on an error object or `false`.

**More than 256 workers get the IP banned.** A worker owns one of 256 nonce lanes; worker
256 + k walked lane k's nonces again, which a server scores as duplicate shares. The worker
count is capped at the lane count, with a note saying why
(`more_workers_than_nonce_lanes_are_capped`).

**A JIT that disagrees with the reference is not noticed.** Mining and `--bench` now check
the JIT against the frozen `SELF_CHECK` vectors first and refuse to start on a mismatch.

**`--bench` misreports why huge pages were refused.** It blamed a missing
`SeLockMemoryPrivilege` for every failure, even under `--no-huge-pages`; it now prints the
allocator's own reason.

**Miner throughput, measured.** On a Ryzen 7 8745HS: 27.12 kH/s with a W^X batch of 4
against 24.88 kH/s at the fixed 32 (`plaine-miner --bench --batch 4` against
`--batch 32`), and 25.0 kH/s pinned against 23.3–23.7 kH/s unpinned (`--no-pin`). Numbers
from other processors are welcome in an issue on this fork.

### Tests

**Integration-test nodes join the real network.** A test config without seeds fell back to
the embedded mainnet seeds, so test nodes dialled them, downloaded the real chain, and had
it reorg the test chain away: the cause of intermittent `ibd.rs` failures, and load on the
real seed nodes on every test run. Test configs set `use_embedded_seeds = false`;
`node/tests/isolation.rs` checks it from the node log.

**`stalled_write_closes_at_deadline` is flaky on Windows.** It relied on loopback socket
buffers staying full, and Windows auto-tunes them under load, so it failed two runs in
three in parallel. It now fixes the buffer sizes and waits until the socket is truly
stalled.

### Repository

**Key files are not ignored.** `.gitignore` did not cover `miner.key`, the file the
README's quick start writes the key to, nor the passphrase files the README documents
(upstream #3).

## Found, not fixed yet

- **Announcements are forgotten on restart.** The list behind `author_getNotes` lives in
  memory and is filled only by blocks connected while the node runs.
- **A note's `time` is always 0.** `NoteIndex::on_block` has no header to take it from.
- **`chainwork` is all zeros below the tip.** Header objects fill it only for the tip.
- **Validator timeouts read as zeros.** When the validator does not answer within two
  seconds, `mempool_getInfo`, `mempool_getBySender`, `account_get.pendingNonce` and parts
  of `node_getBudgets` return 0 or empty instead of an error.
- **Placeholders in answers.** A transaction's `decoded` is always `null`, and
  `chain_getBlockByHeight` at verbosity 2 returns an empty `txs`.
- **The committer after a failed apply.** While building the address index, an error
  returned half-way through applying a block left the storage committer unable to shut
  down (a test hung on drop). The fork checks a block's body before writing anything, so
  the index cannot trigger it; the underlying behaviour was not investigated further.

## Where SPEC.md and the code differ

The code wins; `docs/rpc.md` follows it.

| SPEC says | the node does |
|---|---|
| `emission_audit`: expected issuance "at any height" (§14) | audits the tip only; `issued` is a running total with no per-height record |
| `account_get`: balance, nonce, pendingNonce | also returns `immature` and `spendable` |
| `author_getNotes`: announcements "from a height" | `fromHeight` filters at or below that height, newest first |
| note display: strip, then normalize NFC (§5.5) | the RPC's `text` field strips but does not normalize; SPEC's order is for display clients, which still have to do it |
| `fee_suggest`: percentiles over 240 blocks | now true in this fork; upstream samples nothing |

## What the fork will not change

These are pinned by tests upstream (`node/tests/structure.rs`,
`miner/tests/node_separation.rs`), and the fork keeps them:

- the node depends on `plaine-*` crates and `tokio` and nothing else;
- `Main` is the only network — there is no regtest or testnet profile, so a relaxed
  consensus can never be selected from a config file;
- the node and the wallet `forbid(unsafe_code)`;
- anything with heavier dependencies (the miner, the desktop wallet) lives in its own
  Cargo workspace, and the node's lock file never sees it.

Consensus rules, emission, checkpoints and the reorg limit are out of scope.

## Branches

| branch | purpose |
|---|---|
| `main` | the fork's stable branch and the default; updated from `develop` by pull request |
| `develop` | integration: upstream plus everything in this fork |
| `upstream-main` | mirror of `upstream/main`, fast-forward only |
| `feature/*`, `fix/*`, `docs/*` | work in progress, merged into `develop` |
| `pr/*` | the commits behind upstream PRs #1–#3; no new ones are opened |

## Building

Rust 1.85 or newer.

```
cargo build --release                                     # node and wallet
cargo build --release --manifest-path miner/Cargo.toml    # miner
```

**Windows with the `x86_64-pc-windows-gnu` toolchain.** `getrandom`, `windows-sys` and
friends link through `raw-dylib`, which needs GNU `dlltool` and the assembler it drives.
The `dlltool` rustup ships has no assembler beside it and fails. Get GNU binutils from
[MSYS2](https://www.msys2.org/) (in its UCRT64 shell: `pacman -S
mingw-w64-ucrt-x86_64-binutils`); `scripts/check.sh` finds them and copies just
`dlltool`, `as` and their four DLLs into `target/.tools`, so MSYS2's `gcc` never becomes
the linker. For a manual build, put that directory first on `PATH`.

Do not substitute `llvm-ar` renamed to `dlltool`. It links, and the node and miner even
pass their tests, but the import libraries it writes are wrong for some functions: a
binary that calls one — anything linking `eframe`/`winit`, for instance — dies at start-up
with `STATUS_ACCESS_VIOLATION`. The MSVC toolchain needs none of this.

## Checking

```
scripts/check.sh           # build, all tests, clippy — what every commit must pass
scripts/check.sh --e2e     # plus the slow end-to-end suites
```

See [CONTRIBUTING.md](CONTRIBUTING.md).
