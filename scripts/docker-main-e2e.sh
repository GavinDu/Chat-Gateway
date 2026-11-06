#!/usr/bin/env bash
# Build the actual release image before runtime acceptance; native/debug E2E
# cannot validate the Docker build path. Never run this from an unmerged checkout.
set -euo pipefail
cd "$(dirname "$0")/.."
printf 'Sandbox workspace: '; pwd
if [[ $(git branch --show-current) != main || -n $(git status --porcelain) ]]; then
  echo 'Release Docker E2E requires a clean main checkout.' >&2
  exit 2
fi
commit=$(git rev-parse HEAD)
if [[ -n ${CHAT_GATEWAY_E2E_COMMIT:-} && $commit != "$CHAT_GATEWAY_E2E_COMMIT" ]]; then
  echo 'Main commit differs from CHAT_GATEWAY_E2E_COMMIT; refusing Docker E2E.' >&2
  exit 2
fi
export CHAT_GATEWAY_IMAGE="${CHAT_GATEWAY_IMAGE:-chat-gateway-rs:e2e-${commit:0:12}}"
ulimit -c 0
run() {
  printf 'Sandbox workspace: '; pwd
  printf 'Main commit: %s\n' "$commit"
  "$@"
}
run bash scripts/docker-run.sh build
run docker run --rm "$CHAT_GATEWAY_IMAGE" --help
run bash scripts/docker-runtime-smoke.sh "$CHAT_GATEWAY_IMAGE"
