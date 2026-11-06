#!/usr/bin/env bash
# Shared production link command, also exercised under a strict CI memory cap.
# Dependencies are compiled by the earlier Docker layer; only rebuild this app.
set -euo pipefail
touch src/main.rs
# The builder image supplies the same bounded Clang/LLD RUSTFLAGS to every
# Rust link, including dependency build scripts during a cold build.
exec cargo build --locked --release --features render,stealth
