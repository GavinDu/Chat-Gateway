# chat-gateway-rs — Rust + Obscura

Rust port of GavinDu/Chat-Gateway with **8 providers**: Qwen, Meta AI, Claude, ChatGPT, Gemini, Grok, Mistral, Perplexity.

Unified memory layer + OpenAI-compatible API + automatic failover + Obscura headless browser (30MB, instant boot, built-in stealth).

## Why Obscura over Go Rod + Chromium?

| Metric | Obscura | Headless Chrome + Rod |
|---|---|---|
| Memory | 30 MB | 200+ MB |
| Binary | ~70 MiB | 300+ MB |
| Startup | instant | ~2s |
| Page load | 85ms | ~500ms |
| Anti-detect | Built-in (webdriver=undefined, canvas/GPU/audio randomization, 3520 trackers blocked) | Plugin needed |
| CDP | Yes — `ws://127.0.0.1:9222` — works with chromiumoxide, puppeteer-core, playwright-core | Yes |

Obscura is Apache-2.0, Rust, V8 + custom DOM, single binary. `obscura serve --port 9222 --stealth --render` then `Browser::connect("ws://127.0.0.1:9222")` via chromiumoxide.

## Quick Start

```bash
# 1. Install Obscura
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/obscura-x86_64-linux.tar.gz
tar xzf obscura-x86_64-linux.tar.gz
./obscura fetch https://example.com --eval "document.title" # test

# 2. Start gateway
cp config.example.yaml config.yaml
cargo run -- --config config.yaml
# or with Docker (Dockerfile includes obscura)

# Dashboard at http://localhost:8080 handles manual login for all 8 providers
# Sessions -> sessions/<provider>.json (cookies + localStorage), reloaded on change
```

## API (OpenAI compatible)

- `GET /health` — liveness + provider state
- `GET /v1/models` — configured aliases + discovered catalog (handles collision as `provider:model`)
- `POST /v1/chat/completions` — streaming or JSON, supports `X-Conversation-Id` to reuse tab

```bash
curl http://localhost:8080/v1/models
curl http://localhost:8080/v1/chat/completions -H 'Content-Type: application/json'   -d '{"model":"auto","messages":[{"role":"user","content":"Hello"}]}' 
# auto = priority order: qwen(1) < metaai(2) < claude(3) < chatgpt(4) < gemini(5) < grok(6) < mistral(7) < perplexity(8)
# On failure, tries next eligible provider
```

Pass `X-Conversation-Id` from response to reuse browser tab.

## Providers — Adapters Implemented

All adapters implement `ChatProvider` trait: `discover_models` (reads picker without sending message) + `send` (streaming).

### 1. Qwen — chat.qwen.ai
- Input: `textarea[placeholder*="Ask"]`
- Models: qwen, qwen3, qwen-plus, qwen-max, discovered like qwen3-235b
- Streaming via `div.markdown-body`

### 2. Meta AI — www.meta.ai
- Input: `div[contenteditable][role=textbox]`
- Models: meta-ai, llama-4, llama-3.3
- Llama 4 variants

### 3. Claude — claude.ai
- Input: `div.ProseMirror`
- Models: claude-sonnet-4, opus-4, haiku-3.5
- Detects `data-is-streaming` + settle wait 800ms

### 4. ChatGPT — chatgpt.com / chat.openai.com
- Input: `#prompt-textarea` (contenteditable)
- Models: gpt-5, gpt-4o, o1, o3-mini
- Response: `div[data-message-author-role=assistant] div.markdown`

### 5. Gemini — gemini.google.com
- Input: `rich-textarea`
- Models: gemini-2.0-flash, 1.5-pro
- Custom element `<model-response>`

### 6. Grok — grok.com + x.com/i/grok
- Input: `textarea[placeholder*=Ask]`
- Models: grok-3, grok-3-mini, grok-2
- Supports thinking traces, similar UI to ChatGPT

### 7. Mistral — chat.mistral.ai (Le Chat)
- Input: `textarea.chat-input`
- Models: mistral-large, small, mixtral, codestral, pixtral
- Model picker top

### 8. Perplexity — perplexity.ai
- Input: `textarea[name=q]`
- Models: sonar, sonar-pro, sonar-reasoning, pplx-8b/70b
- Answer with citations — adapter strips `[1][2]` for clean API
- Focus modes + model selector

## Architecture

```
ObscuraSupervisor (spawns obscura serve --stealth)
  -> BrowserPool (chromiumoxide Browser::connect(ws://...), per-provider Semaphore(2))
    -> SessionStore (sessions/*.json -> localStorage injection)
    -> ProviderRouter (priority + model alias + X-Conversation-Id affinity)
      -> ChatProvider adapters (8) -> streaming ChunkStream
        -> UnifiedMemory (DashMap, sliding_window 8k, tab reuse)
          -> Axum SSE /v1/chat/completions
```

Performance traces preserved: parse, memory, context, routing, browser acquisition, queue, readiness, input, send, TTFT, generation (first-update latency, growth, settle, final delivery).

## Obscura Migration from Rod

- `page.MustElement(sel).MustClick()` -> `page.find_element(sel).await?.click().await?` with fallback list
- `WaitStable()` -> `wait_for_selector` + mutation observer polling 350ms
- Screencast: `Page.startScreencast` -> dashboard canvas relay, no CDP/DOM exposure
- Stealth: no plugins needed, Obscura sets `navigator.webdriver=undefined` automatically

## Testing Adapters

```bash
cargo run -- --test-adapter qwen        # discovery + selector check (may send test prompt)
cargo run -- --test-adapter grok
cargo run -- --test-adapter perplexity
```

Dashboard also shows provider state, request activity, config, discovered models, model catalog age/error.

## Security

Session files + browser profiles are auth material. Keep private, restrict fs, don't expose publicly without reverse proxy.

## Next

- Implement cookie persistence via CDP Network.setCookie (currently localStorage only)
- Add summarization strategy for memory beyond sliding_window
- Add Playwright-style tracing export
