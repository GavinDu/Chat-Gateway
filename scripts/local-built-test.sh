#!/usr/bin/env bash
# Run the Chat-Gateway checks locally before pushing. Mirrors .github/workflows/ci.yml.
set -Eeuo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

FIX_FORMAT=0
SKIP_DOCKER=0
SKIP_VENDOR=0
SKIP_DENY=0
SKIP_BENCH=0
REQUIRE_DOCKER=0

usage() {
  cat <<'USAGE'
Usage: scripts/local-built-test.sh [options]

Runs extension validation, Rust formatting, Clippy, tests, benchmark smoke tests,
and (when available) the Docker build/entrypoint check.

Options:
  --fix              Apply cargo fmt first, then verify formatting.
  --skip-docker      Skip Docker image build and entrypoint check.
  --require-docker   Fail instead of warning if Docker is unavailable.
  --skip-vendor      Skip network-dependent vendored-source consistency checks.
  --skip-deny        Skip cargo-deny dependency/license/source audit.
  --skip-bench       Skip benchmark smoke tests.
  -h, --help         Show this help.

Environment:
  CARGO_TARGET_DIR   Cargo build output directory (optional).

Examples:
  scripts/local-built-test.sh
  scripts/local-built-test.sh --fix --skip-docker
  scripts/local-built-test.sh --skip-vendor --skip-bench
USAGE
}

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
warn() { printf '\n\033[1;33mWARNING: %s\033[0m\n' "$*" >&2; }
die() { printf '\n\033[1;31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"; }
run() { log "$*"; "$@"; }

while (($#)); do
  case "$1" in
    --fix) FIX_FORMAT=1 ;;
    --skip-docker) SKIP_DOCKER=1 ;;
    --require-docker) REQUIRE_DOCKER=1 ;;
    --skip-vendor) SKIP_VENDOR=1 ;;
    --skip-deny) SKIP_DENY=1 ;;
    --skip-bench) SKIP_BENCH=1 ;;
    -h|--help) usage; exit 0 ;;
    *) die "Unknown option: $1 (use --help)" ;;
  esac
  shift
done

need cargo
need rustc
need node
need git
need diff
need tar
need grep

log "Repository: $ROOT_DIR"
printf 'Rust: '; rustc --version
printf 'Cargo: '; cargo --version
printf 'Node: '; node --version

log "Validate extension JavaScript, tests, and manifest"
run node --check extension/background.js
run node --check extension/shared.js
run node --check extension/popup.js
run node --test extension/background.test.cjs
run node --test extension/tests/*.test.cjs
run node -e "JSON.parse(require('fs').readFileSync('extension/manifest.json', 'utf8'))"

if ((FIX_FORMAT)); then
  run cargo fmt --all
fi
run cargo fmt --all -- --check
run cargo clippy --all-targets --locked -- -D warnings

if ((!SKIP_DENY)); then
  if cargo deny --version >/dev/null 2>&1; then
    run cargo deny check --all-features
  else
    die "cargo-deny is required for the dependency/license/source audit. Install with: cargo install cargo-deny"
  fi
fi

if ((!SKIP_VENDOR)); then
  need curl
  log "Verify Obscura vendored crates match the pinned Git revision"
  rev="$(sed -n 's/.*obscura-core = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' Cargo.toml)"
  [[ -n "$rev" ]] || die "Could not read the pinned obscura-core revision from Cargo.toml"
  vendor_repo="${TMPDIR:-/tmp}/chat-gateway-obscura-vendor-check"
  rm -rf "$vendor_repo"
  run git clone --quiet --filter=blob:none --no-checkout https://github.com/h4ckf0r0day/obscura.git "$vendor_repo"
  run git -C "$vendor_repo" checkout --quiet "$rev"
  run diff -ru vendor/taffy "$vendor_repo/vendor/taffy"
  run diff -ru vendor/cosmic-text "$vendor_repo/vendor/cosmic-text"

  log "Verify vendored wreq-util matches its published source plus documented patch"
  archive="${TMPDIR:-/tmp}/chat-gateway-wreq-util.crate"
  upstream="${TMPDIR:-/tmp}/chat-gateway-wreq-util-upstream"
  rm -rf "$upstream"
  run curl --fail --silent --show-error --location \
    "https://crates.io/api/v1/crates/wreq-util/3.0.0-rc.12/download" --output "$archive"
  mkdir -p "$upstream"
  run tar -xzf "$archive" -C "$upstream"
  upstream_dir="$upstream/wreq-util-3.0.0-rc.12"
  [[ -d "$upstream_dir" ]] || die "Published wreq-util archive did not contain expected directory"
  run diff -ru --exclude=Cargo.toml --exclude=Cargo.toml.orig --exclude=.cargo_vcs_info.json --exclude=.github --exclude=.gitignore --exclude=CHANGELOG.md --exclude=Cargo.lock --exclude=examples --exclude=release-plz.toml --exclude=rustfmt.toml --exclude=tests --exclude=PATCHES.md vendor/wreq-util "$upstream_dir"
  grep -Fq 'version = "3.0.0-rc.12"' vendor/wreq-util/Cargo.toml || die "Unexpected vendored wreq-util version"
  grep -Fq 'zstd = { version = "0.14.3", optional = true }' vendor/wreq-util/Cargo.toml || die "Expected zstd 0.14.3 patch is missing"
  grep -Fq '"zstd 0.14.3"' Cargo.lock || die "Cargo.lock does not pin zstd 0.14.3"
  if grep -Fq '"zstd 0.13.3"' Cargo.lock; then die "Unexpected zstd 0.13.3 lockfile entry"; fi
fi

run cargo test --locked

if ((!SKIP_BENCH)); then
  run cargo bench --profile dev --locked --bench memory_sqlite -- --smoke
  run cargo run --locked --example provider_bench -- --fake --smoke
fi

if ((!SKIP_DOCKER)); then
  if command -v docker >/dev/null 2>&1; then
    run docker build --pull --tag chat-gateway-rs:local .
    run docker run --rm --entrypoint /usr/local/bin/chat-gateway-rs chat-gateway-rs:local --help
  elif ((REQUIRE_DOCKER)); then
    die "Docker was required but is not installed or not on PATH"
  else
    warn "Docker unavailable; skipped the container build and entrypoint check. Use --require-docker to make this fatal."
  fi
fi

log "All requested local checks passed"
