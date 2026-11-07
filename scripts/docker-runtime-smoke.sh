#!/usr/bin/env bash
# Runtime smoke for the exact image built by CI. No provider sessions or external
# credentials are used; the vault key is a throwaway per-container test value.
set -Eeuo pipefail
set +x

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${1:-chat-gateway-rs:ci}"
NAME="chat-gateway-ci-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}-$$"
# Colima shares the user home, but not macOS /var/folders (default TMPDIR).
# Keep local scratch files in a location available to the Docker VM.
TMP="$(mktemp -d "${HOME:?HOME is required}/.chat-gateway-smoke.XXXXXXXX")"
KEY_VOLUME="$NAME-key"
CONFIG_VOLUME="$NAME-config"
DATA_VOLUME="$NAME-data"
SESSION_VOLUME="$NAME-sessions"
NETWORK="$NAME-network"
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker volume rm "$KEY_VOLUME" "$CONFIG_VOLUME" "$DATA_VOLUME" "$SESSION_VOLUME" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

command -v docker >/dev/null 2>&1
command -v curl >/dev/null 2>&1
command -v python3 >/dev/null 2>&1

# Exercise the actual start helper with an owner-only host config. Direct
# docker-run fixtures missed the nonroot/host-UID config mount failure.
cp "$ROOT/config.example.yaml" "$TMP/config.yaml"
chmod 600 "$TMP/config.yaml"
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
export PORT CHAT_GATEWAY_IMAGE="$IMAGE" CHAT_GATEWAY_CONTAINER="$NAME"
export CHAT_GATEWAY_DATA_VOLUME="$DATA_VOLUME" CHAT_GATEWAY_SESSION_VOLUME="$SESSION_VOLUME"
export CHAT_GATEWAY_KEY_VOLUME="$KEY_VOLUME" CHAT_GATEWAY_CONFIG_VOLUME="$CONFIG_VOLUME"
export CHAT_GATEWAY_NETWORK="$NETWORK" CHAT_GATEWAY_CONFIG="$TMP/config.yaml"
export CHAT_GATEWAY_EXTENSION_ID=abcdefghijklmnopabcdefghijklmnop
bash "$ROOT/scripts/docker-run.sh" start > "$TMP/start.log" 2>&1 || {
  cat "$TMP/start.log" >&2
  exit 1
}
# Verify that the helper did not weaken host-config permissions.
python3 - "$TMP/config.yaml" <<'CONFIGPY'
import os, stat, sys
assert stat.S_IMODE(os.stat(sys.argv[1]).st_mode) == 0o600
CONFIGPY
BINDING="$(docker port "$NAME" "$PORT/tcp")"
if [[ ! "$BINDING" =~ ^127\.0\.0\.1:([0-9]{1,5})$ ]]; then
  echo "Docker published a non-loopback or malformed port binding" >&2
  exit 1
fi
PORT="${BASH_REMATCH[1]}"
if (( PORT < 1 || PORT > 65535 )); then
  echo "Docker published an invalid port" >&2
  exit 1
fi
ORIGIN="http://127.0.0.1:$PORT"

health_ok=0
for _ in $(seq 1 60); do
  status="$(curl --silent --show-error --max-time 3 -D "$TMP/health.headers" \
    -o "$TMP/health.body" -w '%{http_code}' "$ORIGIN/health" 2>/dev/null || true)"
  if [[ "$status" == "200" ]] && grep -Eiq '^x-request-id:[[:space:]]*[^[:space:]]+' "$TMP/health.headers" \
      && python3 - "$TMP/health.body" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    payload = json.load(stream)
raise SystemExit(0 if payload.get("status") == "ok" else 1)
PY
  then
    health_ok=1
    break
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "$NAME" 2>/dev/null || true)" != "true" ]]; then
    echo "Gateway container exited before /health became ready" >&2
    exit 1
  fi
  sleep 1
done
if [[ "$health_ok" != "1" ]]; then
  echo "Docker runtime /health did not return HTTP 200 with a request ID" >&2
  exit 1
fi

ready_status="$(curl --silent --show-error --max-time 3 -D "$TMP/ready.headers" \
  -o "$TMP/ready.body" -w '%{http_code}' "$ORIGIN/ready")"
if [[ "$ready_status" != "503" ]] \
    || ! grep -Eiq '^x-request-id:[[:space:]]*[^[:space:]]+' "$TMP/ready.headers" \
    || ! python3 - "$TMP/ready.body" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    payload = json.load(stream)
raise SystemExit(0 if payload.get("reason") == "no_enabled_provider" else 1)
PY
then
  echo "Docker runtime /ready did not report the expected offline no-enabled-provider state" >&2
  exit 1
fi

# The rotation endpoint must reject requests without the local administrator
# credential; rotated codes must invalidate prior codes and remain single-use.
denied="$(curl --silent --show-error --max-time 5 -o /dev/null -w '%{http_code}' -X POST "$ORIGIN/relay/v1/pairing-code/rotate")"
test "$denied" = "401" || { echo "Unauthenticated pairing rotation was not rejected" >&2; exit 1; }
rotate() {
    CHAT_GATEWAY_CONTAINER="$NAME" CHAT_GATEWAY_KEY_VOLUME="$KEY_VOLUME" PORT="$PORT" \
    bash "$ROOT/scripts/docker-run.sh" pairing-code
}
old_code="$(rotate)"
new_code="$(rotate)"
[[ "$old_code" =~ ^[0-9]{8}$ && "$new_code" =~ ^[0-9]{8}$ ]] || { echo "Invalid rotated pairing code" >&2; exit 1; }
try_pair() {
  curl --silent --show-error --max-time 5 -o "$TMP/pair.body" -w '%{http_code}' \
    -X POST "$ORIGIN/relay/v1/pair" -H 'Origin: chrome-extension://abcdefghijklmnopabcdefghijklmnop' \
    -H 'Content-Type: application/json' -d '{"code":"'"$1"'"}'
}
old_status="$(try_pair "$old_code")"
test "$old_status" = "401" || { echo "Old pairing code remained valid after rotation" >&2; exit 1; }
new_status="$(try_pair "$new_code")"
test "$new_status" = "200" || { echo "New pairing code was not accepted" >&2; exit 1; }
replay_status="$(try_pair "$new_code")"
test "$replay_status" = "401" || { echo "Pairing code could be replayed" >&2; exit 1; }

python3 "$ROOT/scripts/relay-approval-smoke.py" "$ORIGIN" abcdefghijklmnopabcdefghijklmnop

docker stop --time 30 "$NAME" >/dev/null
EXIT_CODE="$(docker inspect --format '{{.State.ExitCode}}' "$NAME")"
if [[ "$EXIT_CODE" != "0" ]]; then
  echo "Gateway container did not exit cleanly after Docker stop (exit $EXIT_CODE)" >&2
  exit 1
fi
if curl --silent --show-error --max-time 2 "$ORIGIN/health" >/dev/null 2>&1; then
  echo "Gateway still accepted HTTP requests after shutdown" >&2
  exit 1
fi

echo "Docker runtime smoke passed: /health=200, /ready=503(no_enabled_provider), request IDs present, real pairing-code CLI rotation and single-use verified, graceful exit=0."
