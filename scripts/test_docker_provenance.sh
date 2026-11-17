#!/usr/bin/env bash
set -Eeuo pipefail
# No Docker daemon, credentials, Git operations or network requests.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/docker" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  info) exit 0 ;;
  buildx)
    if [[ "$2" == version ]]; then exit 0; fi
    if [[ "$2" == build ]]; then printf '%s\n' "$@" > "$MOCK_BUILD_ARGS"; exit 0; fi
    exit 1 ;;
  container) [[ "$2" == inspect ]] ;;
  ps) printf 'mock-container up\n' ;;
  inspect)
    if [[ "$*" == *'.Image'* ]]; then
      printf '%s\n' "$MOCK_IMAGE_ID"
    else
      printf '%s\n' "$MOCK_IMAGE_REV"
    fi ;;
  *) exit 1 ;;
esac
STUB
cat > "$TMP/git" <<'STUB'
#!/usr/bin/env bash
if [[ "$*" == *'status --porcelain'* ]]; then
  [[ "$MOCK_GIT_FAIL" != yes ]] || exit 1
  [[ "$MOCK_GIT_DIRTY" != yes ]] || echo ' M src/main.rs'
elif [[ "$*" == *'rev-parse --verify HEAD'* ]]; then
  printf '%s\n' "$MOCK_SHA"
else
  exit 1
fi
STUB
cat > "$TMP/curl" <<'STUB'
#!/usr/bin/env bash
printf '{"status":"ok","build":{"source_sha":"%s"}}\n' "$MOCK_BINARY_SHA"
STUB
chmod 700 "$TMP/docker" "$TMP/git" "$TMP/curl"
export PATH="$TMP:$PATH"
export MOCK_BUILD_ARGS="$TMP/build-args"
export MOCK_SHA="$(printf 'a%.0s' {1..40})"
export MOCK_IMAGE_ID="sha256:$(printf 'b%.0s' {1..64})"
export MOCK_IMAGE_REV="$MOCK_SHA"
export MOCK_BINARY_SHA="$MOCK_SHA"
export MOCK_GIT_DIRTY=no MOCK_GIT_FAIL=no

bash "$ROOT/scripts/docker-run.sh" build >/dev/null
grep -Fxq "CHAT_GATEWAY_SOURCE_SHA=$MOCK_SHA" "$MOCK_BUILD_ARGS"
grep -Eq '^CHAT_GATEWAY_BUILD_TIMESTAMP=[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$' "$MOCK_BUILD_ARGS"
MOCK_GIT_DIRTY=yes bash "$ROOT/scripts/docker-run.sh" build >/dev/null
grep -Fxq 'CHAT_GATEWAY_SOURCE_SHA=unknown' "$MOCK_BUILD_ARGS"
MOCK_GIT_DIRTY=no MOCK_GIT_FAIL=yes bash "$ROOT/scripts/docker-run.sh" build >/dev/null
grep -Fxq 'CHAT_GATEWAY_SOURCE_SHA=unknown' "$MOCK_BUILD_ARGS"

bash "$ROOT/scripts/docker-run.sh" status > "$TMP/status"
grep -Fxq "Running image ID: $MOCK_IMAGE_ID" "$TMP/status"
grep -Fxq "Image revision: $MOCK_SHA" "$TMP/status"
grep -Fxq "Binary revision: $MOCK_SHA" "$TMP/status"
grep -Fxq 'Build provenance: match' "$TMP/status"

MOCK_BINARY_SHA="$(printf 'c%.0s' {1..40})" bash "$ROOT/scripts/docker-run.sh" status > "$TMP/status"
grep -Fxq 'Build provenance: mismatch' "$TMP/status"

MOCK_IMAGE_REV='https://private.example/token=DO_NOT_EXPOSE' bash "$ROOT/scripts/docker-run.sh" status > "$TMP/status"
grep -Fxq 'Image revision: unknown' "$TMP/status"
grep -Fxq 'Build provenance: unverifiable' "$TMP/status"
if grep -q 'DO_NOT_EXPOSE' "$TMP/status"; then
  echo 'Leaked an untrusted image label' >&2
  exit 1
fi

MOCK_BINARY_SHA='SENSITIVE_COOKIE_SHOULD_NOT_APPEAR' bash "$ROOT/scripts/docker-run.sh" status > "$TMP/status"
grep -Fxq 'Binary revision: unknown' "$TMP/status"
if grep -q 'SENSITIVE_COOKIE_SHOULD_NOT_APPEAR' "$TMP/status"; then
  echo 'Leaked an untrusted health value' >&2
  exit 1
fi
echo 'Docker provenance helper fixtures passed'
