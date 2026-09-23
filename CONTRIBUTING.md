# Contributing to the fork

## Before every commit

```
scripts/check.sh
```

It builds the workspace and the miner, runs every test, and runs clippy with
`-D warnings` on the crates that are clean today. There is no CI: this script is the
gate, and a commit that has not passed it does not go in. Add `--e2e` when the change
touches anything the end-to-end suites cover — the node, the wallet, the miner's stratum
client, the desktop wallet.

## Rules the code keeps

These come from upstream and are enforced by its tests. Do not weaken the tests to get a
change through; if a change needs to break one of them, it belongs in a separate crate.

- **Consensus is off limits.** `lib/consensus`, `lib/pow` and validation change only to
  fix a bug that a test reproduces.
- **Node dependencies:** `plaine-*` and `tokio`. Nothing else.
- **One network.** No regtest, no testnet, nothing that makes a relaxed consensus
  selectable. End-to-end tests run on a fresh chain from the mainnet genesis on local
  ports (see `node/tests/common`).
- **`forbid(unsafe_code)`** in the node and the wallet.
- **`overflow-checks = true`** in release profiles, including new crates.
- **Heavier dependencies live in their own workspace**, as the miner does, with a test
  proving the node's lock file never sees them.

## What a change needs

- [ ] A test that fails without the change.
- [ ] `scripts/check.sh` green; `--e2e` too when relevant.
- [ ] Clippy: no new lints anywhere, including `plaine-noded`, which is reported but not
      gated.
- [ ] New crates are `rustfmt`-clean. Upstream code is not reformatted: it is not
      rustfmt-clean, and reformatting it would turn every sync into conflicts.
- [ ] Documentation updated. A new RPC method goes into `docs/rpc.md`.
- [ ] An entry in `CHANGELOG.md` under *Unreleased*.
- [ ] Anything that mines in a test leaves the machine usable: the node test harness runs
      the miner on all but two cores (`miner_threads()`), and `scripts/check.sh` is
      started at low priority.
- [ ] Nothing is proposed upstream. If the change fixes something upstream also has,
      `FORK.md` describes it: the symptom, the cause, the fix and the test.
- [ ] Everything in the repository is in English: code, comments, docs, commit messages.

## Commit messages

The upstream style: `area: what changed` in the subject, lower case, no trailing period.
The body says why, what was measured or reproduced, and how it was checked. Wrap at 72.

```
miner: cap the worker count at the number of nonce lanes

A worker owns one of 2^THREAD_BITS = 256 nonce lanes ...
```

Areas in use: `node`, `wallet`, `wallet-gui`, `miner`, `rpc`, `storage`, `docs`,
`scripts`, `gitignore`.

## Syncing with upstream

```
git fetch upstream
git checkout main && git merge --ff-only upstream/main
git checkout develop && git merge main
scripts/check.sh --e2e
```

When upstream fixes something the fork also fixed, take upstream's version and drop ours.
