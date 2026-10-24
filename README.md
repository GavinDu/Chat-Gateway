# chat-gateway-rs v2.0.0 - Obscura Native

Rust port of Chat-Gateway with **8 providers**: Qwen, Meta AI, Claude,
ChatGPT, Gemini, Grok, Mistral, Perplexity.

Unified memory layer + OpenAI-compatible API + automatic failover,
running on the **embedded Obscura engine** (the `obscura` crate linked
in-process - no chromiumoxide, no external `obscura serve` CDP process).

## Quick start

```bash
cp config.example.yaml config.yaml
# log in to providers in a browser and save sessions/<provider>.json
# (cookies + localStorage - see Sessions), or let the pool warm without them
cargo run -- --config config.yaml
```

The server listens on `server.port` (default 8080, `PORT` env overrides).

## API (OpenAI compatible)

- `GET /health` - status + configured providers
- `GET /v1/models` - configured aliases + discovered catalog (discovery
  runs in the background at startup; picker labels are read without
  sending a message, failures fall back to the configured aliases)
- `POST /v1/chat/completions` - streaming (SSE) or JSON

```bash
curl http://localhost:8080/v1/models
curl http://localhost:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"Hello"}]}'
# auto = priority order from config; a model alias routes to its provider,
# and on failure the next eligible provider is tried.
```

Pass the `X-Conversation-Id` response header back on later turns to
continue the conversation: context is rebuilt from the unified memory
store (sliding window, ~8k tokens by default) and the conversation keeps
affinity to its last provider.

## Architecture

```
Axum HTTP side (multi-thread)
  -> ObscuraPool (command channel + per-provider semaphores)
    -> browser thread (current-thread runtime + LocalSet)
      -> embedded Obscura Browser, one pre-warmed Page per provider
      -> per-provider mutex serializes sends on its page
      -> provider adapters (selector fallbacks) -> chunk channel back
  -> UnifiedMemory (DashMap, sliding window)
```

Obscura pages wrap a V8 runtime behind interior mutability and are
neither `Clone` nor `Send`, so every page lives on the single browser
thread; the HTTP side only ever sees channels. Provider switch is a
map lookup + channel send on pre-warmed pages - no process or tab
spawn per request.

## Sessions

Sessions live in `sessions/<provider>.json`:
`{ local_storage, cookies: [{name, value, domain, path, secure,
http_only}], ts }`. At warm-up, cookies are restored into the browser's
cookie store **before** the first navigation, then localStorage is
injected after navigation to the provider origin, followed by a
reload-equivalent navigation. Note the cookie store is shared by the
one embedded browser (all providers), which matches how the pages are
warmed.

## Providers

All 8 adapters share one interaction core (`providers/common.rs`):
fallback selector lists for input/send/response/picker, picker-based
model discovery, evaluate-based typing (focus + `insertText`), and a
350ms response-poll loop that yields deltas and ends after ~2s without
growth and no streaming indicator. Qwen's input list is i18n-robust
(English + Chinese placeholder, test-id, contenteditable, bare
`textarea`). Perplexity answers are stripped of `[n]` citation
markers.

Test one adapter live:

```bash
cargo run -- --config config.yaml --test-adapter qwen
```

## Notes / honest limits

- One persistent page per provider: concurrent sends to the same
  provider are serialized on that page (bounded by
  `session_pool_size` permits on the HTTP side).
- The figures in MIGRATION.md (memory, TTFT, switch latency) are the
  v2 design targets from the migration doc, not measurements taken in
  this repo; there is no benchmark harness in the tree yet.
- No dashboard or login UI ships in v2; sessions are captured via the
  pool's save-session path (`ObscuraPool::save_session`) after a manual
  login flow - the endpoints above are the supported surface.
