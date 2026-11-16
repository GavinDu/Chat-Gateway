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

# A previously installed config is not replaced by init; only an explicit
# enable-manus command changes the unique Manus enabled flag.
cat > "$CHAT_GATEWAY_CONFIG" <<'CONFIG'
providers:
  - name: chatgpt
    enabled: false
  - name: manus
    enabled: false # explicitly opted out in the old install
  - name: claude
    enabled: false
server:
  port: 8080
CONFIG
# An owner can inspect only the Manus boolean without exposing any config
# values or starting Docker. The command must be strictly read-only.
cp "$CHAT_GATEWAY_CONFIG" "$TMP/manus-status-before"
[[ "$(bash "$ROOT/scripts/docker-run.sh" manus-status)" == "Manus configuration: disabled" ]]
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/manus-status-before"

bash "$ROOT/scripts/docker-run.sh" enable-manus > "$TMP/manus-message"
grep -Fq "Manus enabled" "$TMP/manus-message"
grep -Fq "Restart Chat Gateway explicitly" "$TMP/manus-message"
grep -Fq "permission to automate" "$TMP/manus-message"
[[ "$(grep -c '^    enabled: true' "$CHAT_GATEWAY_CONFIG")" -eq 1 ]]
grep -Fq 'enabled: true # explicitly opted out in the old install' "$CHAT_GATEWAY_CONFIG"
grep -Fq '  - name: chatgpt' "$CHAT_GATEWAY_CONFIG"
grep -Fq 'server:' "$CHAT_GATEWAY_CONFIG"
[[ "$(bash "$ROOT/scripts/docker-run.sh" manus-status)" == "Manus configuration: enabled (login unverified)" ]]
# Idempotent and tightly scoped: repeated enable does not touch other fields.
cp "$CHAT_GATEWAY_CONFIG" "$TMP/manus-enabled-expected"
bash "$ROOT/scripts/docker-run.sh" enable-manus >/dev/null
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/manus-enabled-expected"

# A lookalike list is not a Manus provider. The old unscoped AWK matcher
# would change this unrelated workflow and falsely report success.
cat > "$CHAT_GATEWAY_CONFIG" <<'CONFIG'
workflows:
  - name: manus
    enabled: false
  - name: nightly-audit
    enabled: true
CONFIG
cp "$CHAT_GATEWAY_CONFIG" "$TMP/decoy-without-provider"
if bash "$ROOT/scripts/docker-run.sh" enable-manus >/dev/null 2>&1; then
  echo "Non-provider Manus workflow was mistaken for the provider" >&2; exit 1
fi
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/decoy-without-provider"

# Only the real top-level providers entry may change. A decoy elsewhere
# must neither block legitimate opt-in nor have its enabled flag touched.
cat > "$CHAT_GATEWAY_CONFIG" <<'CONFIG'
workflows:
  - name: manus
    enabled: false
providers: # explicitly configured provider list
  - name: chatgpt
    enabled: false
  - name: manus
    enabled: false
other:
  - name: manus
    enabled: false
CONFIG
cat > "$TMP/expected-manus-scope" <<'CONFIG'
workflows:
  - name: manus
    enabled: false
providers: # explicitly configured provider list
  - name: chatgpt
    enabled: false
  - name: manus
    enabled: true
other:
  - name: manus
    enabled: false
CONFIG
bash "$ROOT/scripts/docker-run.sh" enable-manus >/dev/null
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/expected-manus-scope" || {
  echo "Enable Manus changed a decoy or failed to change the real provider" >&2
  exit 1
}

# YAML duplicate top-level providers mappings have ambiguous parser semantics.
# Reject rather than edit one of two independent blocks.
cat > "$CHAT_GATEWAY_CONFIG" <<'CONFIG'
providers:
  - name: manus
    enabled: false
plugins:
  - name: something
    enabled: true
providers:
  - name: chatgpt
    enabled: false
CONFIG
cp "$CHAT_GATEWAY_CONFIG" "$TMP/duplicate-providers-section"
if bash "$ROOT/scripts/docker-run.sh" enable-manus >/dev/null 2>&1; then
  echo "Duplicated top-level providers mapping was accepted" >&2; exit 1
fi
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/duplicate-providers-section"

# Unexpected/ambiguous configs must be rejected without modifying the owner
# file or quietly installing a new provider entry.
printf 'providers:\n  - name: chatgpt\n    enabled: false\n' > "$CHAT_GATEWAY_CONFIG"
cp "$CHAT_GATEWAY_CONFIG" "$TMP/no-manus"
if bash "$ROOT/scripts/docker-run.sh" enable-manus >/dev/null 2>&1; then
  echo "Missing Manus config was silently edited" >&2; exit 1
fi
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/no-manus"
printf 'providers:\n  - name: manus\n    enabled: false\n  - name: manus\n    enabled: false\n' > "$CHAT_GATEWAY_CONFIG"
cp "$CHAT_GATEWAY_CONFIG" "$TMP/duplicate-manus"
if bash "$ROOT/scripts/docker-run.sh" enable-manus >/dev/null 2>&1; then
  echo "Duplicate Manus entries were not rejected" >&2; exit 1
fi
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/duplicate-manus"
rm "$CHAT_GATEWAY_CONFIG"
ln -s "$TMP/duplicate-manus" "$CHAT_GATEWAY_CONFIG"
if bash "$ROOT/scripts/docker-run.sh" enable-manus >/dev/null 2>&1; then
  echo "Symlink config was not rejected" >&2; exit 1
fi
rm "$CHAT_GATEWAY_CONFIG"

# Read-only enabled-state status must never print private config bytes,
# discover a decoy Manus workflow, or give an answer for ambiguous YAML.
cat > "$CHAT_GATEWAY_CONFIG" <<'CONFIG'
private:
  gateway_token: SUPER_SECRET_NEVER_OUTPUT
workflows:
  - name: manus
    enabled: false
providers:
  - name: chatgpt
    enabled: false
  - name: manus
    enabled: true # enabled but not proof of authenticated login
CONFIG
cp "$CHAT_GATEWAY_CONFIG" "$TMP/status-secret-config"
[[ "$(bash "$ROOT/scripts/docker-run.sh" manus-status)" == "Manus configuration: enabled (login unverified)" ]]
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/status-secret-config"

assert_redacted_status_failure() {
  if bash "$ROOT/scripts/docker-run.sh" manus-status > "$TMP/status-output" 2>&1; then
    echo "Unverifiable Manus state was reported as enabled or disabled" >&2
    exit 1
  fi
  if grep -Eq 'SUPER_SECRET_NEVER_OUTPUT|gateway_token:|config\.yaml|/tmp/' "$TMP/status-output"; then
    echo "Read-only Manus state command exposed config details" >&2
    exit 1
  fi
  grep -Fq "Manus configuration: " "$TMP/status-output"
}

printf 'workflows:\n  - name: manus\n    enabled: true\nprivate: SUPER_SECRET_NEVER_OUTPUT\n' > "$CHAT_GATEWAY_CONFIG"
cp "$CHAT_GATEWAY_CONFIG" "$TMP/status-decoy-only"
assert_redacted_status_failure
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/status-decoy-only"

printf 'providers:\n  - name: manus\n    enabled: true\nproviders:\n  - name: manus\n    enabled: false\nprivate: SUPER_SECRET_NEVER_OUTPUT\n' > "$CHAT_GATEWAY_CONFIG"
cp "$CHAT_GATEWAY_CONFIG" "$TMP/status-double-section"
assert_redacted_status_failure
cmp -s "$CHAT_GATEWAY_CONFIG" "$TMP/status-double-section"

printf 'providers:\n  - name: manus\n    enabled: false\n  - name: manus\n    enabled: true\nprivate: SUPER_SECRET_NEVER_OUTPUT\n' > "$CHAT_GATEWAY_CONFIG"
assert_redacted_status_failure

printf 'providers:\n  - name: manus\n    enabled: perhaps\nprivate: SUPER_SECRET_NEVER_OUTPUT\n' > "$CHAT_GATEWAY_CONFIG"
assert_redacted_status_failure
printf 'providers:\n  - name: manus\n    enabled: false\n    enabled: true\nprivate: SUPER_SECRET_NEVER_OUTPUT\n' > "$CHAT_GATEWAY_CONFIG"
assert_redacted_status_failure

printf 'providers:\n  - name: manus\n    enabled: true\nprivate: SUPER_SECRET_NEVER_OUTPUT\n' > "$TMP/status-symlink-target"
ln -s "$TMP/status-symlink-target" "$CHAT_GATEWAY_CONFIG"
assert_redacted_status_failure
rm "$CHAT_GATEWAY_CONFIG"
[[ -f "$TMP/status-symlink-target" ]]
grep -Fq SUPER_SECRET_NEVER_OUTPUT "$TMP/status-symlink-target"

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

# The current local-only Docker and popup contract must agree with the
# operator README. Legacy approval is not a prerequisite for local no-pairing.
grep -Fq 'CHAT_GATEWAY_LOCAL_RELAY_NO_PAIRING=1' "$ROOT/scripts/docker-run.sh"
grep -Fq 'connectLocalGateway(gateway, true)' "$ROOT/extension/popup.js"
grep -Fq 'without a pairing code,' "$ROOT/README.md"
grep -Fq 'approval tab, or installation token' "$ROOT/README.md"
if grep -Fq 'approve the local' "$ROOT/README.md"; then
  echo "README incorrectly requires local approval/pairing" >&2
  exit 1
fi

echo "PASS nine provider URLs; existing config unchanged; Manus example/policy and local no-pairing docs consistent"
