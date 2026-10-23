# Chapter 12 — Performance

This chapter describes the implemented request path and its performance characteristics.
It distinguishes measured behavior from proposed work. Measurements are development
observations, not service-level objectives.

## 12.1 Current architecture flow

```text
OpenAI client
  → Gin HTTP handler and request validation
  → optional persisted-history load and message merge
  → model resolution and ordered provider candidates
  → degraded-provider filtering
  → provider-wide browser lock
  → existing conversation tab OR new/warm provider tab
  → session/page readiness and model selection
  → DOM input injection and Send event
  → newest-answer DOM polling across navigation
  → zero-output failure/fallback decision
  → JSON collection OR SSE emission
  → response headers, hooks, history save, dashboard telemetry
```

The API/router path is in-memory and normally cheap. Browser queueing, navigation,
provider generation, selector waits, and response completion dominate latency.

## 12.2 Request flow step by step

1. **Receive input.** `POST /v1/chat/completions` JSON is bound to the OpenAI-shaped
   request. Malformed JSON, missing messages, and unknown models are rejected before
   browser work. The handler reads an optional `X-Conversation-Id`.
2. **Build effective messages.** If a history store is configured, prior messages are
   loaded synchronously and merged with the request. The default no-op store adds no
   work.
3. **Resolve candidates.** Empty or `auto` models use configured priority order. An
   explicit alias places its owning provider first, followed by eligible fallback
   providers. The visible model ID is translated back to the provider label.
4. **Skip degraded providers.** Candidates already marked degraded are skipped. The
   dashboard attempt record is initialized.
5. **Acquire provider capacity.** The provider obtains its lazily created browser entry
   and takes its mutex. The lock is held until the asynchronous response goroutine
   finishes, which serializes all requests to the same provider.
6. **Choose a tab.**
   - A valid provider-scoped conversation ID reuses its retained tab.
   - A new Qwen conversation consumes a warm authenticated tab when available, otherwise
     opens a fresh tab. The warm pool defaults to two and attempts asynchronous
     replenishment after a successful warm-page acquisition; failure and fresh-page
     paths do not yet guarantee capacity restoration.
   - A new Meta AI conversation opens a fresh tab in the existing browser. Prewarming
     Meta tabs was tested and rejected because requests stalled.
7. **Restore/read session state.** On first browser launch, the pool restores cookies
   and localStorage through a temporary provider page. Browser processes are then reused.
8. **Prepare the UI.** A new page waits for bounded load readiness, checks for login
   expiry, and selects a provider model when required. Sequential fallback selectors
   locate the input.
9. **Submit the prompt.** Messages are flattened for the web UI. The adapter injects
   native DOM input/change events, captures the current response baseline, finds Send,
   and dispatches a bounded native click or Enter fallback.
10. **Capture the answer.** Every 200 ms, shared polling evaluates the newest matching
    assistant-answer element. Polling survives SPA document replacement. Text changes
    are emitted as deltas; four seconds without change closes the provider channel.
11. **Decide success or fallback.** A setup error or channel closing with zero output
    before first output marks the provider degraded and advances to the next candidate.
    A failed first provider therefore adds its elapsed time to fallback latency. After
    the first SSE chunk, channel closure is treated as completion even if the answer is
    truncated, because transparent fallback would splice providers.
12. **Write output.**
    - Non-streaming requests concatenate output and return OpenAI-compatible JSON.
    - Streaming requests wait for the first output before committing SSE headers, then
      flush chunks. Fallback is unsafe after the first committed chunk.
13. **Finalize.** The response includes `X-Provider` and `X-Conversation-Id`. The tab is
    retained, success hooks run, and dashboard telemetry records status, duration,
    provider attempts, and fallback state. If a history store is configured and output
    is non-empty, history saving is dispatched asynchronously and is not awaited.

## 12.3 Current provider behavior and measurements

Authenticated Meta AI correction and optimization probes on 2026-09-05 used three
short exact-answer requests in the Replit development container against the local
headless workflow with a manually authenticated Meta AI session. The exact checkpoint
and Chromium version were not captured, so this is an engineering observation rather
than a complete Chapter 12.6 benchmark:

| Stage | Direct success | Exact answers | Mean | Min | Max |
|---|---:|---:|---:|---:|---:|
| Before response-selector correction | 0/3 | 0/3 direct | timeout/fallback | — | — |
| Corrected, stale selector first | 3/3 | 3/3 | 27.43 s | 26.56 s | 28.83 s |
| Corrected, working selector first, browser reused with fresh tabs | 3/3 | 3/3 | 7.87 s | 6.90 s | 8.84 s |

The safe Meta change reduced mean latency by about 71%, or approximately 3.5 times.
Qwen warm requests previously improved from roughly 32–37 seconds to about 8.7 seconds
with browser reuse, a two-tab pool, current selectors, and bounded native DOM events.

A later whitespace-sensitive coding probe found an important integrity limitation:
rendered browser text can omit Python indentation, and a longer Qwen response returned
empty after timeout. Meta AI produced correct code when asked for a whitespace-safe
Base64 payload; Qwen required local repair. These are correctness findings, not latency
passes, and show why performance tests must validate full output rather than HTTP 200
alone.

## 12.4 Verified bottlenecks and risks

### P0 — Correctness and observability

1. **Stage timings are deliberately lightweight.** Each request now records parse,
   memory read, context build, routing, browser acquisition, provider queue, page
   readiness, prompt injection, request send, TTFT, generation, and total time.
   Generation is further divided into provider-first-update latency, active response
   growth, response-settle wait, and final delivery, with only an aggregate update
   count. The dashboard retains the latest 500 compact traces in memory, and one
   structured summary is logged per request. Prompt text, response text, cookies,
   and per-token events are never recorded.
2. **Rendered-text fidelity.** DOM `innerText` extraction can lose code indentation or
   other formatting. Faster incomplete output is not a performance success.
3. **Zero-output error channel.** Many adapter failures close the output channel instead
   of returning a typed asynchronous error, making timeout, selector failure, and empty
   provider output harder to distinguish.
4. **Baseline snapshot errors are ignored.** A failed pre-send snapshot can weaken new
   response detection without explicit telemetry.
5. **Streaming cannot transparently recover after first output.** Partial output followed
   by provider failure must terminate or emit an explicit stream error; switching
   providers would splice unrelated answers.

### P1 — Latency and concurrency

6. **Provider-wide serialization.** One mutex covers page setup through full response
   completion. A slow request blocks every request to that provider, even on independent
   tabs.
7. **Sequential selector fallback.** Input and Send selectors are tried in order under
   the provider lock. A stale first selector previously added about 20 seconds to Meta
   requests.
8. **Fixed 200 ms DOM polling.** Every active request repeatedly performs CDP evaluation
   and DOM queries. This survives navigation but can become expensive with concurrency.
9. **Four-second stability heuristic.** A provider pause longer than the stability
   window can truncate a response. Increasing it raises latency; completion should use
   provider generation state when available.
10. **Fresh-tab and first-launch costs.** Meta pays new-tab navigation for new
    conversations. Both providers pay browser launch and session restoration on the
    first request unless startup work has already created the browser.
11. **Fallback multiplies tail latency.** Providers are attempted sequentially. A
    timeout on the preferred provider can consume most of its deadline before the next
    provider starts.

### P2 — Background work and lifecycle

12. **Health checks contend with requests.** Every five minutes each check opens a real
    tab while holding the same provider lock.
13. **Duplicate session refresh paths.** Central and per-browser watchers can both react
    to session-file changes and repeat refresh work.
14. **Fixed sleeps ignore cancellation.** UI settle/model-selection sleeps do not end
    immediately when the client disconnects.
15. **Retained dead tabs.** Conversation lookup updates metadata but does not first prove
    that the Rod page is still live. A crash can occupy memory and fail a later request.
16. **Qwen warm-pool recovery gaps.** Failed creation or closed allocations can reduce
    warm capacity until another successful allocation triggers replenishment.
17. **Priority edits require restart.** Dashboard priority changes update display state
    immediately but not the immutable runtime priority list, potentially preserving a
    slower fallback order until restart.

## 12.5 Recommended improvements

### Phase 1 — Measure and protect correctness

1. Add structured timings for `queue_wait`, `browser_start`, `page_ready`,
   `selector_input`, `selector_send`, `first_dom_change`, `completion`, and each fallback
   attempt. Add Qwen warm-hit/miss and active-tab gauges.
2. Add output-integrity fixtures: multiline code, indentation, lists, Unicode, long
   pauses, and streaming/non-streaming equivalence.
3. Return a typed asynchronous provider outcome containing terminal error, completion
   reason, and whether any output was emitted.
4. Treat baseline-capture failure as an explicit diagnostic and introduce provider-aware
   completion detection before tuning poll intervals.

### Phase 2 — Remove avoidable waits

5. Cache the last successful selector per provider/role, try it first, invalidate it on
   failure, and retain fallback-use telemetry.
6. Replace fixed sleeps with context-aware waits for concrete DOM state.
7. Use fast polling only while output is changing, then adaptive backoff; where possible,
   combine a mutation observer with reattachment after navigation.
8. Stagger health checks, skip them while a request is active, or use a cached/lightweight
   session probe that does not hold the request lock.
9. Debounce session-file changes into one refresh path.

### Phase 3 — Increase safe concurrency

10. Replace the provider-wide request mutex with:
    - a small lock for browser launch/session mutation,
    - a per-conversation lock for retained tabs, and
    - a bounded per-provider semaphore for independent tabs.
11. Define provider-specific capacity. Start conservatively and load-test account/UI
    behavior; browser capability does not guarantee that a provider account supports
    concurrent generations.
12. Validate page liveness on conversation reuse and close/remove dead tabs immediately.
13. Replenish Qwen warm capacity after all allocation failure/close paths. Keep Meta AI
    on fresh tabs unless repeated correctness tests prove a safe prewarm design.

### Phase 4 — Control tail latency

14. Use per-stage and per-attempt deadlines rather than allowing one failed provider to
    consume nearly the entire request budget.
15. Add a configurable fallback budget and expose why each attempt ended.
16. Consider optional response buffering for clients that prefer reliable fallback over
    immediate SSE. Never splice providers after committed output without an explicit
    protocol.

## 12.6 Benchmark method

Every performance report should record:

- date, build/checkpoint, host, Chromium version, headless mode, and provider session;
- provider/model, new versus continued conversation, and Qwen warm hit/miss;
- streaming mode, prompt class, output size, and exact correctness result;
- HTTP status, actual `X-Provider`, fallback attempts, queue/stage timings, time to first
  output, and total duration;
- sample count, successes, failures, mean, median, p95, minimum, and maximum.

Use at least one exact short response, one normal prose response, one long response, and
one whitespace-sensitive code response. Run correctness checks before comparing speed.
Do not combine cold starts, warm new conversations, and retained-tab continuations into
one latency average.