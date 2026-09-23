#!/usr/bin/env bash
# Every check the fork runs before a commit, in one command. There is no CI on
# purpose (GitHub Actions minutes are not spent on this fork), so this script is
# the gate: run it, and do not commit on red.
#
#   scripts/check.sh            build, tests, clippy for the workspace and the miner
#   scripts/check.sh --e2e      also the slow end-to-end suites (real node + miner)
#   scripts/check.sh --quick    skip the release build of the binaries
#
# Works on Linux, macOS and Git Bash on Windows. Two cores are left free for the
# desktop: builds and test harnesses get (logical CPUs - 2) jobs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ws="$(cd "$here/.." && pwd)"
miner="$ws/miner/Cargo.toml"
gui="$ws/wallet-gui/Cargo.toml"

e2e=0
quick=0
for a in "$@"; do
    case "$a" in
        --e2e) e2e=1 ;;
        --quick) quick=1 ;;
        -h|--help) sed -n '2,12p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

# Leave two logical CPUs to whoever is sitting at the machine.
ncpu="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo "${NUMBER_OF_PROCESSORS:-4}")"
jobs=$(( ncpu > 3 ? ncpu - 2 : 1 ))
export CARGO_BUILD_JOBS="$jobs"
export RUST_TEST_THREADS="$jobs"

# windows-gnu: getrandom and windows-sys link through raw-dylib, which needs a
# dlltool. The one rustup ships in self-contained/ cannot run without an
# assembler; llvm-dlltool can, and llvm-ar answers to that role when it is
# invoked under the name dlltool. Build the shim once under target/.
host="$(rustc -vV | sed -n 's/^host: //p')"
if [[ "$host" == *windows-gnu* ]] && ! command -v dlltool >/dev/null 2>&1; then
    sysroot="$(rustc --print sysroot)"
    llvm_ar="$sysroot/lib/rustlib/$host/bin/llvm-ar.exe"
    if [[ ! -f "$llvm_ar" ]]; then
        echo "windows-gnu needs llvm-dlltool: run  rustup component add llvm-tools" >&2
        exit 1
    fi
    shim="$ws/target/.tools"
    mkdir -p "$shim"
    [[ -f "$shim/dlltool.exe" ]] || cp "$llvm_ar" "$shim/dlltool.exe"
    export PATH="$shim:$PATH"
fi

step() { printf '\n==> %s\n' "$*"; }
run()  { step "$*"; "$@"; }

# Crates that pass clippy with -D warnings today. plaine-noded does not (upstream
# carries a few dozen lints there), so it is linted but not gated, and the fork
# must not add to that count.
gated=(-p plaine-consensus -p plaine-pow -p plaine-storage -p plaine-stratum
       -p plaine-p2p -p plaine-chain -p plaine-rpc -p plaine-wallet)

if (( ! quick )); then
    run cargo build --release --workspace
fi
run cargo test  --release --workspace --no-fail-fast
run cargo clippy --release --all-targets "${gated[@]}" -- -D warnings
step "cargo clippy -p plaine-noded (reported, not gated)"
cargo clippy --release --all-targets -p plaine-noded 2>&1 | grep -E '^warning: .* generated' || true

if [[ -f "$miner" ]]; then
    run cargo test   --release --manifest-path "$miner" --no-fail-fast
    run cargo clippy --release --manifest-path "$miner" --all-targets -- -D warnings
fi

# New crates of the fork are held to rustfmt as well. Upstream code is not
# reformatted: it is not rustfmt-clean, and churning it would turn every sync
# with upstream into conflicts.
if [[ -f "$gui" ]]; then
    run cargo fmt    --manifest-path "$gui" -- --check
    run cargo clippy --release --manifest-path "$gui" --all-targets -- -D warnings
    run cargo test   --release --manifest-path "$gui" --no-fail-fast
fi

if (( e2e )); then
    step "end-to-end suites (real plaine-noded and plaine-miner, gentle profile)"
    if [[ -f "$gui" ]]; then
        run cargo test --release --manifest-path "$gui" -- --ignored --test-threads 1
    fi
fi

printf '\nall checks passed\n'
