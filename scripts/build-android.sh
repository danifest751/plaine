#!/usr/bin/env bash
# Builds plaine-miner for Android phones (arm64-v8a, Android 7 and newer) against
# Android's own libc with the NDK, so it resolves host names like any Android
# program. Then:
#
#   adb push miner/target/aarch64-linux-android/release/plaine-miner /data/local/tmp/
#   adb shell chmod 755 /data/local/tmp/plaine-miner
#   adb shell /data/local/tmp/plaine-miner plne1you.phone@eu.rplant.xyz:17190
#
# The NDK is found through ANDROID_NDK_HOME or ANDROID_NDK_ROOT, else the newest
# one under the SDK (ANDROID_HOME, or %LOCALAPPDATA%\Android\Sdk on Windows,
# ~/Android/Sdk elsewhere). Without an NDK, scripts/build-android-static.sh builds
# a static binary that works but needs the pool as an IP address.
set -euo pipefail
cd "$(dirname "$0")/.."

API=${ANDROID_API:-24}

ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
if [[ -z "$ndk" ]]; then
    sdk=${ANDROID_HOME:-}
    if [[ -z "$sdk" && -n "${LOCALAPPDATA:-}" ]]; then
        sdk="$LOCALAPPDATA/Android/Sdk"
    fi
    sdk=${sdk:-$HOME/Android/Sdk}
    ndk=$(ls -d "$sdk"/ndk/* 2>/dev/null | sort -V | tail -1 || true)
fi
if [[ -z "$ndk" || ! -d "$ndk" ]]; then
    echo "no Android NDK found; set ANDROID_NDK_HOME, or use scripts/build-android-static.sh" >&2
    exit 1
fi

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) host=windows-x86_64; ext=.cmd ;;
    Darwin) host=darwin-x86_64; ext= ;;
    *) host=linux-x86_64; ext= ;;
esac
bin="$ndk/toolchains/llvm/prebuilt/$host/bin"
linker="$bin/aarch64-linux-android${API}-clang${ext}"
if [[ ! -e "$linker" ]]; then
    echo "$linker is missing; is $ndk a complete NDK for API $API?" >&2
    exit 1
fi
if [[ "$host" == windows-x86_64 ]]; then
    linker=$(cygpath -w "$linker")
fi

rustup target add aarch64-linux-android >/dev/null
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$linker"
cargo build --release --manifest-path miner/Cargo.toml \
    --target aarch64-linux-android --bin plaine-miner
echo "built miner/target/aarch64-linux-android/release/plaine-miner (NDK $(basename "$ndk"), API $API)"
