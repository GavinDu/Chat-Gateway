feat: implement full cookie persistence via CDP + performance waterfall traces

- SessionStore now captures and applies full cookie jar via:
  - CDP Network.getAllCookies + document.cookie fallback
  - PersistedCookie struct with domain/path/expires/secure/httpOnly/sameSite
  - sessions/<provider>.json stores cookies + localStorage + timestamp
  - apply_to_page injects cookies before navigation + reloads to auth
  - capture_from_page saves after manual login (dashboard secure relay)

- Performance trace overhaul (mirrors Go's original waterfall):
  - phases: parse, memory, context, routing, browser_acquisition, provider_queue, page_readiness, input, send, TTFT, generation
  - generation breakdown: first_update_latency, active_growth, settle_wait, delivery
  - response_update_count, response_len, total_ms
  - structured log target: performance_trace (no prompts/responses/session data)
  - TraceBuffer 500 retained, /api/traces and /api/traces/:n endpoints
  - Dashboard waterfall visualization with colored segments

- BrowserPool now tracks queue_ms for provider queue wait
- Dashboard shows 8 providers + waterfall bars + secure typing relay
- All 8 providers supported: qwen, metaai, claude, chatgpt, gemini, grok, mistral, perplexity

Committed: 2026-10-23T10:14:41.113057Z
