#!/usr/bin/env bash
# Runtime smoke for the exact image built by CI. No provider sessions or external
# credentials are used; the vault key is a throwaway per-container test value.
set -Eeuo pipefail
set +x

IMAGE="${1:-chat-gateway-rs:ci}"
NAME="chat-gateway-ci-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}-$$"
TMP="$(mktemp -d)"
CONTAINER_CREATED=0
cleanup() {
  if [[ "$CONTAINER_CREATED" == "1" ]]; then
    docker rm -f "$NAME" >/dev/null 2>&1 || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

command -v docker >/dev/null 2>&1
command -v curl >/dev/null 2>&1
command -v openssl >/dev/null 2>&1
command -v python3 >/dev/null 2>&1

# Disposable local pairing administrator secret, readable only through the
# isolated test container mount and never printed.
PAIRING_ADMIN_KEY="$(openssl rand -hex 32)"
printf '%s' "$PAIRING_ADMIN_KEY" > "$TMP/pairing-admin.key"
chmod 0644 "$TMP/pairing-admin.key"
VAULT_KEY="$(openssl rand -hex 32)"
docker run --detach \
  --name "$NAME" \
  --publish 127.0.0.1::8080 \
  --log-driver none \
  --mount "type=bind,source=$TMP/pairing-admin.key,target=/run/secrets/pairing-admin.key,readonly" \
  --tmpfs /app/data:rw,nosuid,nodev,noexec,uid=65532,gid=65532,mode=0700 \
  --tmpfs /app/sessions:rw,nosuid,nodev,noexec,uid=65532,gid=65532,mode=0700 \
  --tmpfs /app/browser:rw,nosuid,nodev,noexec,uid=65532,gid=65532,mode=0700 \
  --tmpfs /app/home:rw,nosuid,nodev,noexec,uid=65532,gid=65532,mode=0700 \
  --env HOME=/app/home \
  --env PORT=8080 \
  --env TOKIO_WORKER_THREADS=2 \
  --env CHAT_GATEWAY_DOCKER_LOCAL_BIND=1 \
  --env CHAT_GATEWAY_EXTENSION_ID=abcdefghijklmnopabcdefghijklmnop \
  --env CHAT_GATEWAY_SESSION_STORE=vault \
  --env "CHAT_GATEWAY_VAULT_KEY=$VAULT_KEY" \
  --env CHAT_GATEWAY_MEMORY_PATH=/app/data/conversations.sqlite3 \
  "$IMAGE" --config /config.example.yaml >/dev/null
CONTAINER_CREATED=1
unset VAULT_KEY

BINDING="$(docker port "$NAME" 8080/tcp)"
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
  curl --fail --silent --show-error --max-time 5 -X POST "$ORIGIN/relay/v1/pairing-code/rotate" -H "Authorization: Bearer $PAIRING_ADMIN_KEY" | python3 -c 'import json,sys; print(json.load(sys.stdin)["code"])'
}
old_code="$(rotate)"
new_code="$(rotate)"
unset PAIRING_ADMIN_KEY
[[ "$old_code" =~ ^[0-9]{8}$ && "$new_code" =~ ^[0-9]{8}$ ]] || { echo "Invalid rotated pairing code" >&2; exit 1; }
try_pair() {
  curl --silent --show-error --max-time 5 -o "$TMP/pair.body" -w '%{http_code}' \
    -X POST "$ORIGIN/relay/v1/pair" -H 'Origin: chrome-extension://abcdefghijklmnopabcdefghijklmnop' \
    -H 'Content-Type: application/json' -d "{\\\"code\\\":\\\"$1\\\"}"
}
old_status="$(try_pair "$old_code")"
test "$old_status" = "401" || { echo "Old pairing code remained valid after rotation" >&2; exit 1; }
new_status="$(try_pair "$new_code")"
test "$new_status" = "200" || { echo "New pairing code was not accepted" >&2; exit 1; }
replay_status="$(try_pair "$new_code")"
test "$replay_status" = "401" || { echo "Pairing code could be replayed" >&2; exit 1; }

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

echo "Docker runtime smoke passed: /health=200, /ready=503(no_enabled_provider), request IDs present, graceful exit=0."
