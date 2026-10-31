#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${CHAT_GATEWAY_IMAGE:-chat-gateway-rs:local}"
CONTAINER="${CHAT_GATEWAY_CONTAINER:-chat-gateway}"
DATA_VOLUME="${CHAT_GATEWAY_DATA_VOLUME:-chat-gateway-data}"
SESSION_VOLUME="${CHAT_GATEWAY_SESSION_VOLUME:-chat-gateway-sessions}"
KEY_VOLUME="${CHAT_GATEWAY_KEY_VOLUME:-chat-gateway-vault-key}"
NETWORK="${CHAT_GATEWAY_NETWORK:-chat-gateway-local}"
PORT="${PORT:-8080}"
CONFIG="${CHAT_GATEWAY_CONFIG:-$ROOT/config.yaml}"
EXTENSION_ID="${CHAT_GATEWAY_EXTENSION_ID:-}"

usage() {
  cat <<'EOF'
Chat Gateway Docker helper

Usage:
  scripts/docker-run.sh init
  scripts/docker-run.sh build
  scripts/docker-run.sh start
  scripts/docker-run.sh stop
  scripts/docker-run.sh restart
  scripts/docker-run.sh status
  scripts/docker-run.sh logs
  scripts/docker-run.sh pairing-code
  scripts/docker-run.sh login <provider>

Environment:
  PORT                         Published loopback port (default: 8080)
  CHAT_GATEWAY_EXTENSION_ID    Chrome extension ID from chrome://extensions
  CHAT_GATEWAY_CONFIG          Config file (default: ./config.yaml)
  CHAT_GATEWAY_IMAGE           Image tag (default: chat-gateway-rs:local)

The gateway port is published on 127.0.0.1 only. Provider sessions are stored
in an encrypted vault inside Docker-managed volumes. The vault key is stored
in a separate Docker-managed volume and is never passed as an environment value.
EOF
}

require_docker() {
  command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 1; }
  docker info >/dev/null 2>&1 || { echo "Docker daemon is not reachable" >&2; exit 1; }
}

absolute_config() {
  local dir base
  dir="$(cd "$(dirname "$CONFIG")" && pwd)"
  base="$(basename "$CONFIG")"
  CONFIG="$dir/$base"
}

open_browser() {
  local url="$1"
  if command -v open >/dev/null 2>&1; then
    open "$url"
  elif command -v xdg-open >/dev/null 2>&1; then
    xdg-open "$url" >/dev/null 2>&1 &
  elif command -v powershell.exe >/dev/null 2>&1; then
    powershell.exe -NoProfile -Command "Start-Process '$url'" >/dev/null
  else
    echo "Open this URL in your external browser: $url"
  fi
}

init_config() {
  if [[ -e "$CONFIG" ]]; then
    echo "Config already exists: $CONFIG"
    return
  fi
  cp "$ROOT/config.example.yaml" "$CONFIG"
  chmod 600 "$CONFIG" 2>/dev/null || true
  echo "Created $CONFIG. Enable only providers you are authorized to use."
}

ensure_volumes_and_key() {
  docker volume create "$DATA_VOLUME" >/dev/null
  docker volume create "$SESSION_VOLUME" >/dev/null
  docker volume create "$KEY_VOLUME" >/dev/null
  docker network inspect "$NETWORK" >/dev/null 2>&1 || docker network create "$NETWORK" >/dev/null

  # Generate the 256-bit vault key inside a dedicated Docker volume. The
  # gateway mounts this volume read-only and reads the key from a 0600 file.
  docker run --rm \
    --mount "type=volume,source=$KEY_VOLUME,target=/vault" \
    alpine:3.22 sh -eu -c '
      umask 077
      if [ ! -s /vault/vault.key ]; then
        od -An -N32 -tx1 /dev/urandom | tr -d " \n" > /vault/vault.key.tmp
        chmod 600 /vault/vault.key.tmp
        mv /vault/vault.key.tmp /vault/vault.key
      fi
      test "$(wc -c < /vault/vault.key | tr -d " ")" = 64
      chmod 600 /vault/vault.key
      chown 65532:65532 /vault/vault.key
    ' >/dev/null
}

container_exists() {
  docker container inspect "$CONTAINER" >/dev/null 2>&1
}

start_gateway() {
  require_docker
  absolute_config
  if [[ ! -f "$CONFIG" ]]; then
    echo "Missing $CONFIG. Run '$0 init' and edit it first." >&2
    exit 1
  fi
  if [[ ! "$PORT" =~ ^[0-9]+$ ]] || (( PORT < 1 || PORT > 65535 )); then
    echo "PORT must be between 1 and 65535" >&2
    exit 1
  fi

  ensure_volumes_and_key
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    bash "$0" build
  fi
  if container_exists; then
    docker rm -f "$CONTAINER" >/dev/null
  fi

  local args=(
    run -d --name "$CONTAINER" --restart unless-stopped --network "$NETWORK"
    -p "127.0.0.1:$PORT:$PORT"
    -e "PORT=$PORT"
    -e "CHAT_GATEWAY_DOCKER_LOCAL_BIND=1"
    -e "CHAT_GATEWAY_SESSION_STORE=vault"
    -e "CHAT_GATEWAY_VAULT_KEY_FILE=/run/secrets/vault.key"
    --mount "type=bind,source=$CONFIG,target=/config.yaml,readonly"
    --mount "type=volume,source=$DATA_VOLUME,target=/app/data"
    --mount "type=volume,source=$SESSION_VOLUME,target=/app/sessions"
    --mount "type=volume,source=$KEY_VOLUME,target=/run/secrets,readonly"
  )
  if [[ -n "$EXTENSION_ID" ]]; then
    args+=(-e "CHAT_GATEWAY_EXTENSION_ID=$EXTENSION_ID")
  fi
  args+=("$IMAGE" --config /config.yaml)
  docker "${args[@]}" >/dev/null

  local startup_timeout="${CHAT_GATEWAY_STARTUP_TIMEOUT_SECONDS:-60}"
  if [[ ! "$startup_timeout" =~ ^[0-9]+$ ]] || (( startup_timeout < 1 || startup_timeout > 600 )); then
    echo "CHAT_GATEWAY_STARTUP_TIMEOUT_SECONDS must be between 1 and 600" >&2
    exit 1
  fi
  local health_url="http://127.0.0.1:$PORT/health"
  local deadline=$((SECONDS + startup_timeout))
  until curl --fail --silent --show-error "$health_url" >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      echo "Chat Gateway did not become healthy within ${startup_timeout}s." >&2
      echo "The container is left running for diagnosis. Inspect it with '$0 logs' or '$0 status'." >&2
      exit 1
    fi
    sleep 1
  done

  echo "Chat Gateway is healthy at http://127.0.0.1:$PORT"
  echo "Container: $CONTAINER"
  echo "Persistent data: $DATA_VOLUME, $SESSION_VOLUME"
  echo "Encrypted-vault key: $KEY_VOLUME (mounted read-only; key is not printed)"
  if [[ -z "$EXTENSION_ID" ]]; then
    echo "Set CHAT_GATEWAY_EXTENSION_ID to enable the Chrome cookie relay."
  fi
  echo "Pairing code: run '$0 pairing-code' (expires 10 minutes after startup)."
}

case "${1:-}" in
  init)
    init_config
    ;;
  build)
    require_docker
    docker build --pull --tag "$IMAGE" "$ROOT"
    ;;
  start)
    start_gateway
    ;;
  stop)
    require_docker
    docker stop "$CONTAINER" >/dev/null 2>&1 || true
    ;;
  restart)
    require_docker
    docker stop "$CONTAINER" >/dev/null 2>&1 || true
    docker rm "$CONTAINER" >/dev/null 2>&1 || true
    start_gateway
    ;;
  status)
    require_docker
    if container_exists; then
      docker ps --filter "name=^/$CONTAINER$" --format 'table {{.Names}}\\t{{.Status}}\\t{{.Ports}}'
      if curl --fail --silent "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
        echo "Health: healthy"
      else
        echo "Health: unhealthy (inspect with '$0 logs')" >&2
        exit 1
      fi
    else
      echo "Container $CONTAINER is not created."
    fi
    ;;
  logs)
    require_docker
    docker logs --follow "$CONTAINER"
    ;;
  pairing-code)
    require_docker
    docker logs "$CONTAINER" 2>&1 | sed -n 's/.*Browser relay pairing code.*: //p' | tail -n 1
    ;;
  login)
    require_docker
    provider="${2:-}"
    case "$provider" in
      chatgpt) url="https://chatgpt.com/" ;;
      claude) url="https://claude.ai/" ;;
      gemini) url="https://gemini.google.com/" ;;
      grok) url="https://grok.com/" ;;
      qwen) url="https://chat.qwen.ai/" ;;
      metaai) url="https://www.meta.ai/" ;;
      mistral) url="https://chat.mistral.ai/" ;;
      perplexity) url="https://www.perplexity.ai/" ;;
      *) echo "Usage: $0 login <chatgpt|claude|gemini|grok|qwen|metaai|mistral|perplexity>" >&2; exit 2 ;;
    esac
    echo "Sign in in your normal external browser, complete MFA/CAPTCHA there, then use the Chat Gateway Relay extension to link $provider."
    echo "The extension transfers provider cookies to the encrypted Docker vault; passwords are never collected by the gateway."
    open_browser "$url"
    ;;
  *)
    usage
    exit 2
    ;;
esac
