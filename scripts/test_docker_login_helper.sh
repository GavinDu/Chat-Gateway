#!/usr/bin/env bash
set -Eeuo pipefail

# No Docker daemon, provider website, browser session or credential is used.
# Fake only the helper's existence/health prerequisite and URL opener.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/docker" <<'DOCKERSTUB'
#!/usr/bin/env bash
[[ "${1:-}" == "info" ]]
DOCKERSTUB
cat > "$TMP/open" <<'OPENSTUB'
#!/usr/bin/env bash
[[ "$#" == 1 ]] || exit 1
printf '%s\n' "$1" > "$CHAT_GATEWAY_TEST_OPEN_DEST"
OPENSTUB
chmod 700 "$TMP/docker" "$TMP/open"
export PATH="$TMP:$PATH"
export CHAT_GATEWAY_TEST_OPEN_DEST="$TMP/visited-url"

providers=(chatgpt claude gemini grok qwen metaai mistral perplexity manus)
urls=(
  https://chatgpt.com/
  https://claude.ai/
  https://gemini.google.com/
  https://grok.com/
  https://chat.qwen.ai/
  https://www.meta.ai/
  https://chat.mistral.ai/
  https://www.perplexity.ai/
  https://manus.im/app
)
for ((i=0; i<${#providers[@]}; i++)); do
  rm -f "$CHAT_GATEWAY_TEST_OPEN_DEST"
  bash "$ROOT/scripts/docker-run.sh" login "${providers[i]}" >/dev/null
  actual="$(cat "$CHAT_GATEWAY_TEST_OPEN_DEST")"
  [[ "$actual" == "${urls[i]}" ]] || {
    echo "Provider ${providers[i]} opened the wrong official sign-in URL" >&2
    exit 1
  }
done

# Reject arbitrary external locations and never silently open a guessed URL.
if bash "$ROOT/scripts/docker-run.sh" login unknown-provider >/dev/null 2>&1; then
  echo "Unregistered login provider was accepted" >&2
  exit 1
fi
[[ "$(cat "$CHAT_GATEWAY_TEST_OPEN_DEST")" == "https://manus.im/app" ]]

# Re-running init must not overwrite the owner's existing provider selection.
export CHAT_GATEWAY_CONFIG="$TMP/config.yaml"
printf 'owner-existing-configuration\n' > "$CHAT_GATEWAY_CONFIG"
bash "$ROOT/scripts/docker-run.sh" init >/dev/null
[[ "$(cat "$CHAT_GATEWAY_CONFIG")" == "owner-existing-configuration" ]]

echo "PASS all nine provider login URLs; unknown provider denied; existing config unchanged"
