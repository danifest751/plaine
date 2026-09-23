# This fork

`danifest751/plaine` is a fork of [noaltitude/plaine](https://github.com/noaltitude/plaine).
It tracks upstream and adds a desktop wallet, the node and wallet work that wallet needs,
and fixes to the reference miner. Consensus is not changed here: a block or transaction
valid for upstream is valid for the fork and the other way round.

The plan and its status are in [docs/ROADMAP.md](docs/ROADMAP.md) (Russian — it is the
working plan of the maintainer). Everything meant for users and contributors is in English.

## What differs from upstream

Every change is listed in [CHANGELOG.md](CHANGELOG.md). In short, as of now:

**Miner**

- `mining.submit` echoes the job id exactly as the server sent it, and the stratum reader
  understands every spelling of an accepted share (`true`, an object, a bare
  `error: null`). Without this the miner cannot mine on pools that do not pad job ids to
  eight hex digits — rplant.xyz among them. *Proposed upstream as noaltitude/plaine#1.*
- The worker count is capped at the 256 nonce lanes; more workers would repeat each
  other's nonces and get the IP banned for duplicate shares. *Upstream #2.*
- The JIT is checked against the frozen `SELF_CHECK` vectors before mining and before a
  benchmark. *Upstream #2.*
- `--bench` reports why huge pages were not obtained in the allocator's own words.
  *Upstream #2.*
- The W^X batch is sized to the machine's L2 (`--batch N` to override), and workers are
  pinned one per core by default (`--no-pin` to opt out). Together about +27 % on the
  machine it was measured on. *Fork only for now.*

**Repository**

- `.gitignore` covers `miner.key`, the name the upstream README's quick start writes the
  key to, and the passphrase files the README documents. *Upstream #3.*

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
| `main` | mirror of `upstream/main`, fast-forward only |
| `develop` | integration: upstream plus everything in this fork; releases are cut from here |
| `feature/*`, `fix/*`, `docs/*` | work in progress, merged into `develop` |
| `pr/*` | single commits on top of `upstream/main`, each proposed upstream as a PR |

## Building

Rust 1.85 or newer.

```
cargo build --release                                     # node and wallet
cargo build --release --manifest-path miner/Cargo.toml    # miner
```

**Windows with the `x86_64-pc-windows-gnu` toolchain.** `getrandom` and `windows-sys`
link through `raw-dylib` and need a `dlltool`. The one rustup ships cannot run without an
assembler. `scripts/check.sh` handles this by itself; for a manual build:

```
rustup component add llvm-tools
mkdir -p target/.tools
cp "$(rustc --print sysroot)/lib/rustlib/x86_64-pc-windows-gnu/bin/llvm-ar.exe" target/.tools/dlltool.exe
PATH="$PWD/target/.tools:$PATH" cargo build --release
```

`llvm-ar` behaves as `llvm-dlltool` when it is invoked under that name. The MSVC toolchain
does not need any of this.

## Checking

```
scripts/check.sh           # build, all tests, clippy — what every commit must pass
scripts/check.sh --e2e     # plus the slow end-to-end suites
```

See [CONTRIBUTING.md](CONTRIBUTING.md).
