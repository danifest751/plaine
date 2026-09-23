# Changelog

Changes in this fork relative to [upstream](https://github.com/noaltitude/plaine) at
`189598d`. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Unreleased

### Added

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

### Fixed

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
