# go-proxy

An OpenAI-compatible local proxy for **Qwen**, **Meta AI**, **Claude**,
**ChatGPT**, and **Gemini** web chat sessions.
It automates Chromium through Go, Rod, and the Chrome DevTools Protocol (CDP).

## Requirements

- Go 1.22+ to build
- Chrome or Chromium available on the host
- A manually authenticated account for each provider you enable

## Build

```bash
go build -o go-proxy .
# Apple Silicon macOS package:
./build-macos-arm64.sh
```

## Start

Copy the supplied configuration, manually save a session for one or more supported
providers, then start the server:

```bash
cp config.example.yaml config.yaml
./go-proxy --login qwen --config config.yaml
# In the headed browser, complete login yourself; stop the command when finished.
./go-proxy --config config.yaml
```

The dashboard at `http://localhost:8080/` starts an isolated manual-login browser and
streams its page into the login modal. Clicks, scrolling, keyboard controls, and text
are relayed to that browser without exposing CDP or the provider DOM. Login capture
saves cookies and localStorage; authentication input and screenshots are not logged or
persisted. On mobile, tap the provider field first, then use the secure typing relay
below the browser view; its Next and Enter buttons relay those keys to the provider.

Supported login names are `qwen`, `metaai`, `claude`, `chatgpt`, and `gemini`.
Sessions are stored in
`sessions/<provider>.json` and are reloaded when changed.

## Configuration

```yaml
server:
  port: 8080
  headless: true
  chrome_profile_dir: ""

providers:
  - name: qwen
    enabled: true
    priority: 1
    timeout_seconds: 120
    session_pool_size: 2
    models: [qwen]
  - name: metaai
    enabled: true
    priority: 2
    timeout_seconds: 120
    models: [meta-ai, llama]
  - name: claude
    enabled: true
    priority: 3
    timeout_seconds: 120
    models: [claude, claude-sonnet, claude-opus, claude-haiku]
  - name: chatgpt
    enabled: true
    priority: 4
    timeout_seconds: 120
    models: [gpt, chatgpt, gpt-5, gpt-4o]
  - name: gemini
    enabled: true
    priority: 5
    timeout_seconds: 120
    models: [gemini, gemini-pro, gemini-flash]
```

`PORT` overrides `server.port`. Lower priority values are tried first. Model aliases
are case-insensitive; `"auto"` uses the priority order.

At startup, the proxy reads the authenticated model or bot picker on each enabled
provider page without sending a message. Successful discoveries are combined with the
configured aliases, saved to `models-cache.json` beside the config file, and refreshed
at most once every 24 hours. If discovery fails, the last successful catalog remains
active; on a first-run failure, the configured aliases remain active.

Unique discovered labels are exposed directly. If multiple providers expose the same
label, `/v1/models` qualifies only those collisions as `provider:model` so every
advertised ID routes deterministically to its `owned_by` provider. Configured aliases
keep their existing unqualified IDs when a discovered label collides with them.

## API

- `GET /health` — liveness and provider state
- `GET /v1/models` — configured aliases plus the last discovered provider models
- `POST /v1/chat/completions` — OpenAI-style streaming or JSON chat response

```bash
curl http://localhost:8080/v1/models

curl http://localhost:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen","messages":[{"role":"user","content":"Hello"}]}'
```

Pass `X-Conversation-Id` from a successful response to reuse its browser tab for a
later turn. On a failed provider the proxy tries the next eligible provider.

## Dashboard and diagnostics

The dashboard shows provider state, request activity, configuration, discovered model
options, and the age or error state of each provider's model catalog. Its Login
action is manual only and works remotely without host desktop access.
`--test-adapter <provider>` performs a live selector check; it is diagnostic and
may send a test prompt.

Expand a request in the live log to inspect its low-overhead latency waterfall:
parse, memory, context, routing, browser acquisition, provider queue, page readiness,
input, send, TTFT, generation, and total time. Generation is divided into provider
first-update latency, active response growth, response-settle wait, and final delivery,
plus an aggregate response-update count. The latest 500 traces are retained in memory
and each request emits one structured `performance trace` log entry. Prompts,
responses, session data, and per-token events are not recorded.

The repeatable beta checklist and the latest credential-free beta execution record
are in [`design/11-beta-test-plan.md`](design/11-beta-test-plan.md).
The current end-to-end request flow, measured provider behavior, performance
bottlenecks, and prioritized improvements are in
[`design/12-performance.md`](design/12-performance.md). The complete design index is
[`design/README.md`](design/README.md).

## Security

Session files and browser profiles are authentication material. Keep them private,
restrict filesystem access, and do not expose the proxy publicly without an
appropriate reverse proxy and access controls.

## Scope

Production adapters are Qwen, Meta AI, Claude, ChatGPT, and Gemini. Playwright
migration is deferred; the current automation implementation remains Go + Rod over CDP.