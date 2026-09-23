#!/usr/bin/env bash
# Builds every release binary from a clean checkout and packs the archives:
#
#   scripts/package-release.sh v1.0.0-fork.1
#
# Out of dist/<version>/:
#   plaine-<version>-windows-x86_64.zip    node, desktop wallet, wallets, miner, scripts
#   plaine-<version>-linux-x86_64.tar.gz   node, wallet, miner (static, musl)
#   plaine-<version>-android-arm64.zip     the miner for Android phones (NDK)
#   SHA256SUMS
#
# Runs on Windows (Git Bash, GNU toolchain) and builds the Linux binaries with the
# rust-lld that ships with Rust, and the Android one with the NDK. Every binary is
# built with PLAINE_REQUIRE_BUILD_ID=1: a build that cannot name its commit fails.
set -euo pipefail
cd "$(dirname "$0")/.."

version=${1:?usage: scripts/package-release.sh <version, e.g. v1.0.0-fork.1>}
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    echo "the working tree has changes; release builds come from a clean checkout" >&2
    exit 1
fi
export PLAINE_REQUIRE_BUILD_ID=1
jobs=$(( $(nproc) > 3 ? $(nproc) - 2 : 1 ))
out="dist/$version"
rm -rf "$out"
mkdir -p "$out"

if [[ -d target/.tools ]]; then
    export PATH="$PWD/target/.tools:$PATH"
fi

echo "== windows x86_64"
cargo build --release -j "$jobs" --bin plaine-noded --bin plaine-wallet
cargo build --release -j "$jobs" --manifest-path miner/Cargo.toml --bin plaine-miner
cargo build --release -j "$jobs" --manifest-path wallet-gui/Cargo.toml

echo "== linux x86_64 (static musl)"
rustup target add x86_64-unknown-linux-musl >/dev/null
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes -C target-feature=+crt-static"
cargo build --release -j "$jobs" --target x86_64-unknown-linux-musl --bin plaine-noded --bin plaine-wallet
cargo build --release -j "$jobs" --manifest-path miner/Cargo.toml --target x86_64-unknown-linux-musl --bin plaine-miner

echo "== android arm64 (NDK)"
bash scripts/build-android.sh

echo "== packing"
py=python3
"$py" -c 'import sys' 2>/dev/null || py=python
"$py" - "$version" "$out" <<'PY'
import io, os, sys, tarfile, zipfile
version, out = sys.argv[1], sys.argv[2]

def text(path, crlf):
    s = io.open(path, encoding="utf-8").read().replace("\r\n", "\n").replace("{VERSION}", version)
    return (s.replace("\n", "\r\n") if crlf else s).encode("utf-8")

def zipped(name, files):
    with zipfile.ZipFile(os.path.join(out, name + ".zip"), "w", zipfile.ZIP_DEFLATED) as z:
        for arc, src, kind in files:
            info = zipfile.ZipInfo(f"{name}/{arc}", date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (0o755 if kind == "bin" else 0o644) << 16
            data = open(src, "rb").read() if kind == "bin" else text(src, kind == "crlf")
            z.writestr(info, data)

w = "target/release"
zipped(f"plaine-{version}-windows-x86_64", [
    ("plaine-noded.exe", f"{w}/plaine-noded.exe", "bin"),
    ("plaine-wallet.exe", f"{w}/plaine-wallet.exe", "bin"),
    ("plaine-miner.exe", "miner/target/release/plaine-miner.exe", "bin"),
    ("plaine-wallet-gui.exe", "wallet-gui/target/release/plaine-wallet-gui.exe", "bin"),
    ("plaine-wallet-cli.exe", "wallet-gui/target/release/plaine-wallet-cli.exe", "bin"),
    ("noded.toml", "scripts/release/noded.toml", "crlf"),
    ("start-node.bat", "scripts/release/start-node.bat", "crlf"),
    ("mine-pool.bat", "scripts/release/mine-pool.bat", "crlf"),
    ("README.txt", "scripts/release/README-windows.txt", "crlf"),
    ("LICENSE-MIT.txt", "LICENSE-MIT", "crlf"),
])

name = f"plaine-{version}-linux-x86_64"
l = "target/x86_64-unknown-linux-musl/release"
with tarfile.open(os.path.join(out, name + ".tar.gz"), "w:gz") as t:
    for arc, src, kind in [
        ("plaine-noded", f"{l}/plaine-noded", "bin"),
        ("plaine-wallet", f"{l}/plaine-wallet", "bin"),
        ("plaine-miner", "miner/target/x86_64-unknown-linux-musl/release/plaine-miner", "bin"),
        ("noded.toml", "scripts/release/noded.toml", "lf"),
        ("README.txt", "scripts/release/README-linux.txt", "lf"),
        ("LICENSE-MIT", "LICENSE-MIT", "lf"),
    ]:
        data = open(src, "rb").read() if kind == "bin" else text(src, False)
        info = tarfile.TarInfo(f"{name}/{arc}")
        info.size, info.mtime = len(data), 1767225600
        info.mode = 0o755 if kind == "bin" else 0o644
        t.addfile(info, io.BytesIO(data))

zipped(f"plaine-{version}-android-arm64", [
    ("plaine-miner", "miner/target/aarch64-linux-android/release/plaine-miner", "bin"),
    ("README.txt", "scripts/release/README-android.txt", "lf"),
    ("LICENSE-MIT", "LICENSE-MIT", "lf"),
])
PY

(cd "$out" && sha256sum *.zip *.tar.gz > SHA256SUMS)
echo "== done"
ls -l "$out"
cat "$out/SHA256SUMS"
