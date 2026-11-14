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

# Keep the checked-in provider risk register truthful about the owner
# example. This does not certify that any provider terms permit automation.
awk '
  /^  - name: / { provider=$3 }
  /^    enabled: / {
    if (provider == "manus" && $2 == "true") manus_enabled=1
    if (provider != "manus" && $2 == "true") unexpected_enabled=1
  }
  END { if (!manus_enabled || unexpected_enabled) exit 1 }
' "$ROOT/config.example.yaml" || {
  echo "Shipping example must enable only Manus" >&2
  exit 1
}
grep -Fq 'enables **Manus** only' "$ROOT/docs/PROVIDERS_POLICY.md" || {
  echo "Policy document disagrees with the shipping example" >&2
  exit 1
}
grep -Fq 'not blanket permission to automate' "$ROOT/docs/PROVIDERS_POLICY.md" || {
  echo "Manus example must not be misrepresented as terms authorization" >&2
  exit 1
}

echo "PASS nine provider URLs; existing config unchanged; Manus example/policy consistent"
