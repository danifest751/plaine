# Fork roadmap and implementation plan

The working plan for `danifest751/plaine`: a desktop wallet, the node and wallet work it
needs, miner follow-ups, tests up to end-to-end runs, documentation, and releases. This is
a living document; the *Status* section is updated after every phase.

---

## 1. Starting point

As found on 2026-09-23, before any work.

**Repositories.** Upstream `noaltitude/plaine` is a single commit, `189598d` ("initial
public release"), untouched since 2026-09-16. Three PRs from the fork are open upstream:
#1 (stratum: echo the job id verbatim, parse object results), #2 (worker cap, JIT
self-check, the real reason huge pages were refused), #3 (`.gitignore` for key files).

**Tests.** About 2,100 `#[test]` functions in the workspace. A baseline release run on
Windows of the node, wallet and RPC crates: 597 tests, 0 failures, including tests that
start a real `plaine-noded` and mine. The miner: 181 tests.

**An end-to-end harness already exists.** `node/tests/common` starts a real
`plaine-noded` on local ports, builds the miner, and mines a fresh chain from the mainnet
genesis to a target height. Existing tests mine 2–4 blocks; none mines past coinbase
maturity (60 blocks) and spends the reward.

**Rules upstream pins with tests** (`node/tests/structure.rs`,
`miner/tests/node_separation.rs`). The fork keeps all of them:

| rule | consequence for the fork |
|---|---|
| node dependencies are `plaine-*` and `tokio` only | no new libraries in the node |
| `config::Network` is `Main` only; "regtest" may not appear in shipped code | no test network; end-to-end tests run on a fresh chain from the mainnet genesis |
| `#![forbid(unsafe_code)]` in the node and the wallet | nothing with `unsafe` goes into those crates |
| the miner is a separate workspace the node cannot see | the desktop wallet is built the same way |
| `overflow-checks = true` in release | kept in every new crate |

**Gaps.**
- RPC has no per-address history: balance, nonce and mempool entries are available,
  confirmed incoming and outgoing transfers are not. That blocks a wallet with a UI.
- SPEC §14 lists the RPC methods in a table without parameters or response schemas.
- There is no CI, and there will be none: GitHub Actions minutes are not spent on this
  fork. The gate is the local `scripts/check.sh`, run before every commit.
- Stratum listens on `0.0.0.0:9258` by default, so a solo node exposes its mining server.
- The miner's topology detection does not know `target_os = "android"`.
- The wallet KDF (`blake3-iter-v1`) is not memory-hard, as its own source says.

---

## 2. Principles

1. **Consensus is off limits.** `lib/consensus`, `lib/pow` and validation change only to
   fix a bug that a test reproduces. Network parameters, emission, checkpoints and the
   reorg limit do not change.
2. **Upstream's rules are kept, not worked around.** Anything that would break them lives
   in a separate crate or workspace.
3. **Everything stays in this repository.** Nothing is proposed upstream: no issues, PRs
   or comments beyond the three PRs already open. Whatever upstream could use is written
   up in `FORK.md` instead — each defect with its symptom, cause, fix and test — so anyone
   can take it from there.
4. **Every change comes with** a test that fails without it, updated documentation and a
   `CHANGELOG.md` entry.
5. **Load tests leave the machine usable.** Anything that mines in a test leaves two cores
   free, and the check script runs at low priority; full power only for final
   measurements, and announced.
6. **English only.** Code, comments, documentation, commit messages and PRs.

---

## 3. Branches and process

```
main          the fork's stable branch and the repository's default; changes arrive
              from develop through a pull request, after scripts/check.sh --e2e
develop       integration: upstream plus everything in the fork
upstream-main mirror of upstream/main, fast-forward only
feature/*   new functionality  -> merged into develop
fix/*       fixes              -> merged into develop
pr/*        the commits behind the three PRs opened upstream earlier; no new ones
docs/*      documentation
```

- Commit messages follow upstream: `area: what changed`; the body says why and how it was
  checked.
- Syncing: `upstream-main` fast-forwards to upstream, then `develop` merges it; when
  upstream fixes something the fork also fixed, upstream's version wins.
- Releasing to `main`: a pull request from `develop` whose description lists what it
  brings, merged with a merge commit once `scripts/check.sh --e2e` is green.
- Fork versions are tags `v1.0.0-fork.N`; the log is `CHANGELOG.md` (Keep a Changelog).

---

## 4. Phases

Estimates are working days, with margin. Phases 3 and 4 run in parallel.

### Phase 0. Foundation — 1–2 days

| # | task | done when |
|---|---|---|
| 0.1 | `develop`: merge the three PR branches and the miner tuning; split the mixed tuning commit into "batch sizing" and "default pinning" | `develop` builds and every test passes |
| 0.2 | No CI (owner's decision: GitHub minutes are not spent). Instead `scripts/check.sh`: build, all tests, clippy `-D warnings` on clean crates, `--e2e` for the slow suites | the script is green on Windows |
| 0.3 | Lint policy: upstream is not rustfmt-clean (2,000+ differences) and is not reformatted; `fmt --check` applies to new crates only. Clippy gates every library and the miner; `plaine-noded` is reported, and must not get worse | recorded in the script and `CONTRIBUTING.md` |
| 0.4 | Windows build: GNU `dlltool` from MSYS2, set up by `scripts/check.sh`, documented in `FORK.md` | a clean machine builds by the instructions |
| 0.5 | **Measure:** how long mining 61 blocks on a fresh chain takes in the gentle profile | sets the budget for end-to-end tests |
| 0.6 | **Spike:** `egui_kittest` on Windows without a display — click a button, read a label | confirmed, or a fallback chosen |
| 0.7 | `FORK.md`, `CONTRIBUTING.md`, `CHANGELOG.md` | in place |

### Phase 1. Node: what the wallet needs — 4–6 days

**1.1 Address index and transaction history.** Opt-in, like `txindex`:
`addrindex = true`. A table of "address → (height, tx index)" written when a block is
connected. Rows are not deleted on a reorg: as with `txindex`, a reader checks each hit
against the body that is canonical at that height, so rows from a replaced block drop out
by themselves. A new RPC method `account_getHistory(address, limit, cursor)` with cursor
paging in the style of `author_getNotes`. Without the index it fails with an explanation,
as `tx_get` does without `txindex`.

Tests: storage (order, paging, reorg, restart, enabling it later); RPC (paging, empty
history, index off); end-to-end on a real node (mine → the coinbase shows up in history).

**1.2 Transaction status for the wallet.** "In the mempool / in block N / unknown" for the
wallet's own transactions, without a full `txindex`: history plus `mempool_getBySender`.

**1.3 Safe stratum default.** `127.0.0.1:9258` by default, `0.0.0.0` written explicitly
in the config for miners on other machines or a pool front-end, with a hint in the log.
It changes a default, so `CHANGELOG.md` and `FORK.md` call it out for anyone upgrading.

**1.4 RPC reference** — `docs/rpc.md`: parameters, response schemas, error codes, `curl`
examples. A test checks that every method in `methods.rs` is documented.

**1.5 Fee suggestions from the chain.** `fee_suggest` returns the relay floor for every
percentile and samples no blocks, although SPEC §14 promises percentiles over recent
blocks. The wallet's fee field needs real numbers: sample the transfers of the last blocks,
fall back to the floor when there are none, and say how many blocks were sampled.

**1.6 RPC defects found while writing the reference.** Each gets a failing test first, then
the fix, then an entry in `FORK.md`:

- after a reorg, `author_getNotes` loses the announcements of the new branch: the notes of
  the applied blocks are added and then removed by the rollback that should precede them;
- `chain_getBlockByHash` with a side-branch hash returns the canonical block's body at that
  height under the side header;
- with `txindex` on and the body pruned, `tx_get` says the index begins at that height
  when the cause is pruning;
- the announcement list lives in memory and is empty after a restart, and a note's `time`
  is always 0;
- a header's `chainwork` is all zeros except at the tip.

**Deliberately not done:** regtest, new consensus parameters, a different seed list.

### Phase 2. Wallet core for the GUI — 2–3 days

**2.1 `wallet::api` facade.** Create, import and open a key, build and sign a transfer,
parse amounts — without printing to stdout (`ui.rs` and `wallet_cli.rs` print today). The
CLI moves onto the same facade; its behaviour does not change, which the existing 141
tests check.

**2.2 KDF v2.** A new file format `kdf: argon2id-v1` with ~64 MiB of memory. Existing
`blake3-iter-v1` files open as before; migration through `passphrase`. The cost is a new
dependency in the wallet, against upstream's minimalism; the alternative is to keep v1 and
have the GUI generate strong passphrases by default. Decided before phase 2 starts.

**2.3 Tests:** the facade; known-answer vectors for KDF v2; backward compatibility — v1 keys
created by the current version open after the change.

### Phase 3. Desktop wallet — 8–10 days

**Architecture.**

```
wallet-gui/                  its own [workspace], like miner/
  src/rpc/     RPC client; a NodeApi trait so tests can substitute the node
  src/model/   state and logic without egui: amounts, validation, the send flow
  src/ui/      egui views
  tests/       UI tests (kittest) and end-to-end tests
```

- **eframe / egui 0.36.** Pure Rust, one binary for Windows, Linux and macOS, no WebView
  and no JavaScript build. Above all, `egui_kittest` of the same version drives the UI in
  tests without a display. For a wallet, having no browser engine is also less attack
  surface.
- **RPC client:** a minimal HTTP/1.1 client over `std::net` for a local node. Requests run
  on a background thread; the UI never blocks.
- **Works with any node, upstream's included.** Consensus is unchanged, so balance,
  sending and pending transactions work against an upstream `plaine-noded`. What an
  older node or one without `addrindex` cannot answer degrades instead of failing: the
  GUI probes `account_getHistory` once and, on "method not found" or "feature disabled",
  shows the history of the wallet's own sends from a sent-transfer log it keeps beside
  the key file (the CLI's journal records announcements only), each confirmed once the
  account nonce has moved past it and it is gone from the mempool, says incoming
  transfers need a node with `addrindex = true`, and falls back to the relay floor for
  fees. A test runs the model against a mock shaped like upstream's node.
- A separation test, like `node_separation.rs`: the node's and the wallet's lock files
  contain no egui.

**Screens in the first version.**

1. **First run:** create a key, import the 68-digit recovery string, open a key file. A
   passphrase field with a strength estimate; a warning for `kdf: none`.
2. **Home:** the address (copy, QR code); balance — spendable / maturing / pending; node
   status — height, sync, peers.
3. **Send:** address checked as bech32m while typing; amount in PLNE; fee from
   `fee_suggest` with a choice of level; nonce taken automatically
   (`account_get.pendingNonce`); a confirmation screen; sign, `tx_sendRaw`, journal entry.
4. **History:** confirmed from `account_getHistory`, pending from the mempool and the
   journal.
5. **Settings:** node address, RPC token, language, backup — the recovery string is shown
   only after the passphrase is entered again.

**Security.** Secrets live only in `Secret32`, which zeroes itself; auto-lock on a timer;
secrets never reach logs; the clipboard is cleared after the recovery string is copied;
the key never leaves the process.

### Phase 4. Tests and end-to-end — alongside phase 3, finished 3–4 days after

| level | what | how |
|---|---|---|
| unit | `model` logic without the UI: amounts, validation, sending | `#[test]` |
| UI | every screen, transitions, input errors | `egui_kittest` + a substituted `NodeApi` |
| integration | the RPC client against a real node | a harness modelled on `node/tests/common` |
| end-to-end, main path | the whole scenario through the GUI (below) | real `plaine-noded` and `plaine-miner` |
| end-to-end, failures | insufficient funds, bad address, wrong passphrase, node down, reused nonce | same |

**Main end-to-end scenario:**

```
1. start plaine-noded on a fresh chain (local ports, no seeds, addrindex = true)
2. in the GUI, create key A and key B
3. plaine-miner mines to A in the gentle profile until the reward matures (61+ blocks)
4. the GUI shows A's spendable balance, equal to account_get
5. in the GUI, send A -> B and confirm with the passphrase
6. mine one more block
7. check: B has the balance and an incoming history entry, A an outgoing one;
   the sum of all balances agrees with emission_audit
```

The scenario is slow — minutes; the exact figure comes from measurement 0.5 — so it runs
under `scripts/check.sh --e2e`. The fast tests run on every commit.

**Done when** every level is green under `scripts/check.sh --e2e`.

### Phase 5. Miner — 3–4 days

| # | task |
|---|---|
| 5.1 | PRs #1–#3 stay open upstream as they are; their changes live in `develop` either way |
| 5.2 | Batch sizing and default pinning: the measurements, and how to reproduce them, in `FORK.md` |
| 5.3 | Android: `topo.rs` on `any(target_os = "linux", target_os = "android")`, plus a test |
| 5.4 | Machine-readable status: `--status-format json`, one JSON line per tick, as the GUI's data source |
| 5.5 | A Mining tab in the GUI: start and stop `plaine-miner` as a child process, "background" and "maximum" profiles, hash rate and shares from the JSON status, pool or solo |
| 5.6 | End-to-end: the GUI starts the miner against a local node and sees accepted shares |

### Phase 6. Release — 2 days

- Builds for Windows and Linux; a portable archive like the miner kit.
- Release builds with `PLAINE_REQUIRE_BUILD_ID=1` — the mechanism in `build.rs` already
  exists: a binary that cannot name its commit is not built.
- `SHA256SUMS` with every release.
- A user guide with screenshots.

---

## 5. Deliberately out of scope

| what | why |
|---|---|
| regtest or testnet in the node | upstream forbids it by test: a relaxed consensus must never be selectable from a config file |
| changes to consensus, emission, checkpoints | not the fork's to make |
| TLS in the miner | upstream refuses it on purpose; stunnel if needed |
| a web wallet or browser extension | extra attack surface |
| hardware wallets, multisig | not before the first GUI version has settled |

---

## 6. Open decisions

| # | question | recommendation |
|---|---|---|
| 1 | GUI framework | egui/eframe — decided: testable through kittest, one binary, no WebView |
| 2 | KDF v2 on argon2id | yes if a new wallet dependency is acceptable; otherwise strong generated passphrases in the GUI |
| 3 | Stratum on `127.0.0.1` by default | decided: changed in the fork, called out in `CHANGELOG.md` and `FORK.md` |
| 4 | GUI licence | MIT, as upstream |
| 5 | UI languages | English first; translations as separate resource files |

---

## 7. Risks

| risk | response |
|---|---|
| the maturity end-to-end run takes too long | measurement 0.5; if needed, `--e2e` only before merging into `develop` |
| `egui_kittest` does not work on Windows without a display | resolved by spike 0.6: it works |
| the address index misbehaves on reorg and pruning | rows are verified at read time like `txindex`; storage tests cover reorg, restart and enabling late |
| upstream diverges from the fork | small commits, regular syncs of `main` |
| Windows build friction | GNU binutils from MSYS2, set up by `scripts/check.sh` |

---

## 8. Definition of done for a change

- [ ] a test that fails without the change
- [ ] `scripts/check.sh` green (`--e2e` when the change touches the node, wallet, miner or GUI)
- [ ] upstream's structure tests are not weakened
- [ ] documentation updated; a new RPC method goes into `docs/rpc.md`
- [ ] a `CHANGELOG.md` entry
- [ ] anything that mines in a test does so in the gentle profile
- [ ] if it fixes something upstream also has, `FORK.md` describes it
- [ ] everything is in English

---

## 9. Timeline

| phase | days |
|---|---|
| 0. Foundation | 1–2 |
| 1. Node | 4–6 |
| 2. Wallet core | 2–3 |
| 3. GUI | 8–10 |
| 4. Tests and end-to-end | alongside 3, plus 3–4 |
| 5. Miner | 3–4 |
| 6. Release | 2 |
| **total** | **about 5 weeks** |

## 10. Status

- [x] **Phase 0** — done. `develop` holds the three upstream PRs and the miner tuning in two
  commits; `scripts/check.sh` replaces CI; `FORK.md`, `CONTRIBUTING.md`, `CHANGELOG.md`
  written; `egui_kittest` confirmed on Windows without a display. Found and fixed on the
  way: integration-test nodes were dialling the real mainnet seeds and syncing the real
  chain (the cause of flaky `ibd.rs`); a stratum test depended on Windows not auto-tuning
  loopback buffers; `llvm-ar` standing in for `dlltool` writes broken import libraries.
  A clean rebuild runs 2,100 tests with 0 failures. Measurement 0.5 moves to phase 4,
  where the harness that needs it is written.
- [ ] Phase 1 — in progress. 1.1 done: `addrindex` and `account_getHistory`, with storage,
  RPC and real-node tests; the method is documented in `docs/rpc.md`, which 1.4 extends.
  1.2 done: `tx_get(txid, address)` answers without `txindex`, and a real transfer is
  followed end to end (`transfer_roundtrip.rs`, under `check.sh --e2e`).
  1.4 done: `docs/rpc.md` covers all 19 methods, checked field by field against a running
  node; `reference_doc.rs` keeps it in step with the method list. Writing it turned up the
  defects listed under 1.6. 1.3 done: stratum listens on loopback unless the config
  opens it, and the log says how.
  1.6, first part done: announcements survive a reorg, a side-branch hash no longer gets
  another block's body, and a pruned `tx_get` names pruning. Still open: announcements
  after a restart, a note's `time`, `chainwork` below the tip.
  1.5 done: `fee_suggest` reads the transfer fees of the last 240 blocks, cached per tip;
  checked by unit tests and by the transfer end-to-end run.
- [ ] Phase 2 — in progress. 2.1 done: `plaine_wallet::api` creates, imports, opens,
  signs, backs up and re-wraps keys without printing, and returns advice as `Notice`s;
  the CLI runs on it with unchanged output (its 141 tests pass as before) and
  `wallet/tests/api.rs` covers the library face. 2.2 waits on the KDF decision.
- [ ] Phase 3
- [ ] Phase 4
- [ ] Phase 5
- [ ] Phase 6
