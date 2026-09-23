#!/usr/bin/env bash
# Builds plaine-miner as a static aarch64 Linux binary that runs on Android phones
# (arm64-v8a) without the NDK: musl libc, linked by the rust-lld that ships with
# Rust. Copy it to the phone and run it from a shell there:
#
#   scripts/build-android-static.sh
#   adb push miner/target/aarch64-unknown-linux-musl/release/plaine-miner /data/local/tmp/
#   adb shell chmod 755 /data/local/tmp/plaine-miner
#   adb shell /data/local/tmp/plaine-miner --bench
#
# One limit: musl resolves host names from /etc/resolv.conf, which Android does
# not have, so give the pool or node as an IP address. A build against Android's
# own libc (target aarch64-linux-android, which needs the NDK) resolves names.
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add aarch64-unknown-linux-musl >/dev/null
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes -C target-feature=+crt-static"
cargo build --release --manifest-path miner/Cargo.toml \
    --target aarch64-unknown-linux-musl --bin plaine-miner
echo "built miner/target/aarch64-unknown-linux-musl/release/plaine-miner"
