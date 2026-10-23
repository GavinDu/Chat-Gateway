#!/usr/bin/env bash
set -euo pipefail

BINARY="go-proxy-darwin-arm64"
OUT_DIR="dist"
ARCHIVE="${OUT_DIR}/go-proxy-macos-arm64.tar.gz"

echo "==> Building go-proxy for macOS Apple Silicon (darwin/arm64)..."
mkdir -p "${OUT_DIR}"
GOOS=darwin GOARCH=arm64 go build -o "${OUT_DIR}/${BINARY}" .
echo "    Binary: ${OUT_DIR}/${BINARY} ($(du -sh "${OUT_DIR}/${BINARY}" | cut -f1))"

echo "==> Copying config and README..."
cp config.example.yaml "${OUT_DIR}/config.yaml"

cat > "${OUT_DIR}/README.txt" << 'EOF'
Go Proxy — macOS Quick Start (Apple Silicon)
=============================================

SETUP
  1. Make the binary executable:
       chmod +x go-proxy-darwin-arm64

  2. Clear macOS quarantine (only needed if downloaded, not self-built):
       xattr -dr com.apple.quarantine go-proxy-darwin-arm64

   3. Manually log in to each provider you want to use (opens a headed Chrome window):
       ./go-proxy-darwin-arm64 --login metaai    --config config.yaml
       ./go-proxy-darwin-arm64 --login qwen      --config config.yaml
      Complete login directly in the browser, then stop the command to save cookies
      and localStorage. No raw passwords are stored by go-proxy.

  4. Start the proxy:
       ./go-proxy-darwin-arm64 --config config.yaml

USAGE
  Dashboard:  http://localhost:8080/

  List models:
    curl http://localhost:8080/v1/models

  Chat (non-streaming):
    curl http://localhost:8080/v1/chat/completions \
      -H "Content-Type: application/json" \
       -d '{"model":"qwen","messages":[{"role":"user","content":"Hello!"}]}'

  Multi-turn conversation (reuses browser tab across turns):
    curl http://localhost:8080/v1/chat/completions \
      -H "Content-Type: application/json" \
      -H "X-Conversation-Id: my-chat-1" \
       -d '{"model":"qwen","messages":[{"role":"user","content":"Hello!"}]}'

NOTES
  - Sessions are saved to ./sessions/<provider>.json next to the binary.
  - Requires Chrome/Chromium installed (auto-detected via rod).
  - To suppress browser windows, set headless: true in config.yaml.
EOF

echo "==> Creating archive: ${ARCHIVE}..."
tar -czf "${ARCHIVE}" -C "${OUT_DIR}" "${BINARY}" config.yaml README.txt
echo "    Archive: ${ARCHIVE} ($(du -sh "${ARCHIVE}" | cut -f1))"

echo ""
echo "Done. To test locally:"
echo "  tar -xzf ${ARCHIVE}"
echo "  chmod +x ${BINARY}"
echo "  ./${BINARY} --config config.yaml"
