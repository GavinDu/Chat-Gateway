# Chapter 9 — Backlog

## 9.1 Playwright migration `[DEFERRED]`

Evaluate a move from Rod/CDP to Playwright only when it has a clear operational benefit.
The current supported implementation remains Go + Rod over CDP; no migration is in
progress.

## 9.2 Browser resilience `[PLANNED]`

Add crash recovery, bounded per-provider queues, and clearer session-expiry reporting.

## 9.3 Selector maintenance `[PLANNED]`

Schedule opt-in adapter checks and record successful fallback selector use, so stale
selectors can be reviewed. Checks must use manually authenticated sessions.

## 9.4 Additional providers `[DEFERRED]`

No additional provider is production-supported. Historical adapters for ChatGPT, Gemini,
Claude, Perplexity, and DeepSeek are inactive and removed from supported scope; any
future reintroduction requires a separate verification and manual-session design.

## 9.5 Performance and concurrency `[PLANNED]`

Prioritize observability before raising concurrency:

1. Add per-stage timing, provider queue time, warm-pool hit rate, selector fallback
   counts, response first-change time, and completion time.
2. Replace the provider-wide request lock with per-conversation locks plus a bounded
   per-provider tab semaphore.
3. Reduce 200 ms DOM polling with adaptive backoff or a navigation-aware observer.
4. Cache the last successful selector and make fixed sleeps cancellation-aware.
5. Deduplicate session-file refresh work and move health probes off the request-critical
   browser lock where safe.
6. Validate retained-tab liveness and replenish Qwen warm capacity on every failed or
   closed allocation path.

Meta AI warm-tab pooling is not planned: it was tried and reduced reliability. Its safe
current optimization is browser-process reuse with fresh tabs for new conversations.
Chapter 12 contains the complete analysis and measurement guidance.