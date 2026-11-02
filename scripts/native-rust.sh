#!/usr/bin/env bash
# Reproducible Rust-only environment. No inherited gateway/provider Secrets.
set -euo pipefail
[[ $# -gt 0 ]] || { echo 'Usage: bash scripts/native-rust.sh cargo <arguments>'; exit 2; }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/cg-native-rust.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
umask 077
mkdir -p "$WORK/home" "$WORK/tmp"
BUILD_ENV=()
# Replit/Nix static-TLS and bindgen workarounds; absent on standard Ubuntu.
GCC=/nix/store/2ga5nd1m56n5cx2wh8vbf6nrdhqk2f0q-gcc-15.3.0-lib/lib
CLANG=/nix/store/mmk6s6mc9kvz8czjfg9lh9m4sbc0wc8k-clang-21.1.7-lib/lib
GLIBC=/nix/store/dj43clc5ff7jjnfmhbaj6q4q0h8kpfpm-glibc-2.40-66-dev/include
if [[ -d "$GCC" ]]; then
  BUILD_ENV+=(GLIBC_TUNABLES=glibc.rtld.optional_static_tls=1048576 LD_LIBRARY_PATH="$GCC")
fi
if [[ -d "$CLANG" && -d "$GLIBC" ]]; then
  BUILD_ENV+=(LIBCLANG_PATH="$CLANG"
    "BINDGEN_EXTRA_CLANG_ARGS=-isystem $GLIBC -isystem $CLANG/clang/21/include")
fi
cd "$ROOT"
env -i PATH="$PATH" HOME="$WORK/home" TMPDIR="$WORK/tmp" \
  CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
  RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
  RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" \
  CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}" \
  CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  "${BUILD_ENV[@]}" "$@"
