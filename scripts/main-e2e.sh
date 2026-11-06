#!/usr/bin/env bash
# Real-process acceptance is deliberately restricted to a clean main checkout.
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $# -lt 1 || $# -gt 2 || (${2:-} != '' && ${2:-} != '--http-only') ]]; then
  echo 'Usage: bash scripts/main-e2e.sh /absolute/path/to/chat-gateway-rs [--http-only]' >&2
  exit 2
fi
if [[ $(git branch --show-current) != main || -n $(git status --porcelain) ]]; then
  echo 'E2E requires a clean main checkout. Commit/merge changes and sync main first.' >&2
  exit 2
fi
commit=$(git rev-parse HEAD)
if [[ -n ${CHAT_GATEWAY_E2E_COMMIT:-} && $commit != "$CHAT_GATEWAY_E2E_COMMIT" ]]; then
  echo 'Main commit differs from CHAT_GATEWAY_E2E_COMMIT; refusing E2E.' >&2
  exit 2
fi
binary=$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$1")
[[ -x $binary ]] || { echo 'Gateway binary is missing or not executable.' >&2; exit 2; }
# Prevent generated test credentials from being retained in process core dumps.
ulimit -c 0
run() {
  printf 'Sandbox workspace: '; pwd
  printf 'Main commit: %s\n' "$commit"
  "$@"
}
run node browser-login/http-smoke.mjs "$binary"
if [[ ${2:-} != '--http-only' ]]; then
  run node browser-login/smoke.mjs "$binary"
  run node browser-login/smoke.mjs "$binary" --through-ssh
fi
