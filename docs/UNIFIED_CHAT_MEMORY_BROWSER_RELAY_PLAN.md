# Chat-Gateway: Unified API, Memory, and Browser Relay Improvement Plan

**Status:** Proposed architecture and phased implementation plan  
**Repository reviewed:** `GavinDu/Chat-Gateway`  
**Goal:** One stable chat API and one provider-neutral memory layer across official APIs and browser-backed providers, with the user's real browser serving as the authentication/session relay and Rust/Obscura retained as an optional high-performance execution backend.

## 1. Executive summary

The current project already has a useful foundation: an Axum/Tokio Rust service, an OpenAI-shaped chat-completions endpoint, eight browser-backed provider adapters, an embedded Obscura browser, provider routing/failover, and an in-process memory store.

The key architectural gap is that these pieces are not yet separated by stable contracts. The API can lose conversation history during routing, memory is not reliably persisted or re-injected, provider browser threads can be confused across conversations, and session files are sensitive credentials without adequate isolation. The recommended sequence is:

1. Secure the gateway and session handling.
2. Define a canonical request/event/error contract and a backend trait.
3. Make conversation storage durable, idempotent, and provider-neutral.
4. Use a browser extension/local relay for user-driven login and session lifecycle.
5. Keep Obscura as a pluggable Rust execution backend, not as the owner of user identity.
6. Add conformance tests, provider health signals, and measured performance benchmarks.

## 2. Current state and gaps

Based on the repository README and the accompanying code review, the current service exposes `POST /v1/chat/completions`, `GET /v1/models`, and `GET /health`; it runs provider adapters through a shared Obscura browser thread. The README describes a DashMap sliding-window memory layer and session JSON files under `sessions/`.

Important gaps to verify and address in code:

- API behavior is only partially OpenAI-compatible: full message history, parameter handling, error envelopes, usage, cancellation, and SSE completion semantics need conformance tests.
- Memory is process-local and can duplicate turns; context must be explicitly injected into requests, especially when switching providers.
- Provider web threads need a per-conversation/per-provider mapping. One warm page per provider cannot safely represent arbitrary independent conversations without explicit thread management.
- Streaming completion is inferred from DOM changes and silence, which can truncate long pauses or hang on repeated identical answers.
- Session files contain credential-equivalent cookies/local storage. A browser-wide cookie store and unauthenticated relay endpoints create a high-impact security boundary.
- The repository needs repeatable CI, adapter fixtures, health/readiness semantics, and actual benchmarks before performance targets are treated as achieved.

Treat these as review findings to validate against the current branch before implementation; this document is a plan, not a claim that tests have run.

## 3. Target architecture

```text
OpenAI-compatible clients
        |
   API facade (auth, validation, quotas, request IDs)
        |
 Canonical ChatRequest / ChatEvent / GatewayError
        |
 Router + policy (provider pinning, model aliases, failover)
        |
 Conversation & Memory Service ---- SQLite (WAL, FTS5)
        |                            conversations, messages,
        |                            summaries, provider threads,
        |                            tenant/key ownership
        |
 Backend trait
   |                 |                    |
 Official API     Browser Relay        Obscura Web
 backend           backend              backend
                   (real browser tab)   (Rust embedded engine)
        |
 Provider/session health, metrics, audit-safe logs
```

**Design rule:** provider adapters must not own canonical history, memory policy, API semantics, or authentication. They should translate a canonical request into provider-specific operations and emit canonical events.

## 4. Unified chat API

### Canonical contract

Introduce stable internal types:

- `ChatRequest`: request ID, conversation ID, model/provider preference, ordered messages, supported generation parameters, stream flag, and policy options.
- `ChatEvent`: role, text delta, usage, finish, and typed error events.
- `GatewayError`: stable code, HTTP status, retryability, provider, and safe diagnostic detail.
- `ChatBackend`: capability discovery, health/session status, completion stream, cancellation, and optional conversation/thread operations.

Implement at least two backend families: `ApiBackend` for supported official APIs and `WebBackend` for browser automation. A `RelayBackend` can route execution through the authenticated user's real browser when direct Obscura execution is not supported or reliable.

### API behavior

- Preserve the full ordered message list and system/developer instructions according to each backend's capabilities.
- Use exact model aliases and explicit `provider/model` names. Unknown model names should return `model_not_found`; only an explicit `auto` model should use automatic routing.
- Support provider pinning and explicit fallback policy (off, same-family, or any). Do not silently switch vendors when a caller pinned a model/provider.
- Validate request parameters. Either support them or return a clear unsupported-parameter error rather than silently ignoring them.
- Make SSE event shape consistent, including role, deltas, final finish reason, usage when available, and `[DONE]`. Mark estimates as estimates.
- Detect failures before committing to a stream when possible. After streaming starts, emit a typed stream error; never label a partial response as a successful completed turn.
- Return meaningful timeout and overload errors; propagate client cancellation to the backend.
- Expose capabilities and availability through `/v1/models`, and distinguish liveness (`/health`) from readiness (`/ready`).

## 5. Unified memory layer

### Storage model

Use SQLite in WAL mode as the initial durable store; use FTS5 for lexical search. Keep the data model provider-neutral:

- `conversations`: ID, tenant/key owner, timestamps, title, retention policy.
- `messages`: stable ID, conversation ID, role, content/parts, source, provider/model, timestamps, completion status, request ID.
- `provider_threads`: conversation ID + provider/account, provider thread URL/ID, last synchronized message ID, model, session state.
- `summaries`: conversation ID, covered message range, summary version, updated time.
- `long_term_memories` (optional and off by default): user-approved facts/preferences, provenance, confidence, expiry, and deletion metadata.

All rows must be scoped to the authenticated gateway identity. Conversation IDs are identifiers, not authorization tokens.

### Correctness rules

1. Append each accepted user turn once using request/message IDs and idempotency checks.
2. Reconcile clients that resend their full message history: detect the known stored prefix and append only new messages; do not duplicate the transcript on every request.
3. Persist assistant output as pending while streaming; mark complete only on a successful finish event. Keep interrupted/error responses explicitly incomplete.
4. Build context from a summary plus recent messages under a configured token/character budget. Use a tokenizer when available; keep conservative fallback estimates.
5. Always inject the assembled context into the backend request. A provider switch must not reduce the request to only the latest user message.
6. For web providers, send only the latest user turn when the mapped provider thread is verified and synchronized. On a new thread, provider switch, or out-of-sync thread, send a bounded context preamble plus the latest turn, then update the thread map only after success.
7. Add conversation list/read/delete endpoints and configurable retention. Long-term memory requires explicit user controls to inspect, edit, export, and delete it.

Do not assume that a provider's web thread is the source of truth. The gateway's canonical log is authoritative.

## 6. Browser relay for login and session management

### Intended workflow

The user's normal browser is the identity boundary: the user navigates to the provider and completes login, SSO, MFA, passkey, and any challenge directly with that provider. The gateway should not collect or log passwords. A locally paired browser extension or local companion then establishes an authenticated relay for supported operations.

Preferred sequence:

1. Gateway binds to loopback by default and displays a short-lived, one-time pairing code.
2. The extension verifies the gateway identity and pairs using the code. It stores a revocable per-install token.
3. The user explicitly links a provider and completes authentication in the provider's own browser tab.
4. The extension reports only the minimum session/link state needed by the gateway. Prefer live relay operations over exporting reusable cookies.
5. The relay backend sends prompts to the authenticated tab and forwards response deltas/status to the gateway over an authenticated local channel.
6. The gateway tracks session state (`unlinked`, `linked`, `verified`, `degraded`, `expired`, `needs_login`) and prompts the user to re-authenticate when needed.
7. Unpairing revokes the token and clears cached session material; the user can unlink each provider independently.

### Security requirements

- Never accept arbitrary remote relay calls. Bind to `127.0.0.1` by default; require explicit authentication for every state-changing route.
- Use one-time pairing, origin checks as defense in depth, request nonces/expiry, rate limits, and a narrow route allowlist. Origin checking alone is not authentication.
- Do not expose an unrestricted browser-navigation endpoint. Enforce a strict provider-domain allowlist and validate redirects.
- Treat cookies, local storage, access tokens, and browser session identifiers as passwords. Avoid exporting them whenever possible. If persistence is unavoidable and permitted, encrypt at rest using an OS keychain-backed key, isolate by provider/account, restrict filesystem permissions, and provide immediate revocation/deletion.
- Never log secrets, prompt bodies by default, cookie values, authorization headers, or relay payloads. Redact errors and telemetry.
- Make relay pairing and session import unavailable to remote network clients. Refuse unsafe non-loopback binds unless explicit strong authentication and TLS are configured.
- Do not attempt to bypass provider access controls, CAPTCHAs, device-bound sessions, or anti-bot protections. If a provider disallows automated access or blocks the relay, mark it unsupported and use an official API or a user-operated flow instead.
- Review the applicable provider terms and account risks before enabling each web adapter. Keep the gateway private and single-user until multi-tenant isolation and policy are reviewed.

### Why not simply copy cookies into Obscura?

Cookies are bearer credentials and may be bound to browser/device/network properties. Copying them to another engine can both increase credential exposure and fail unpredictably. The safer default is to keep authentication in the real browser and relay supported user-authorized actions. If a provider explicitly supports a session export/import flow, implement it as a separate opt-in adapter with documented risk and per-provider isolation.

## 7. Obscura's role and performance plan

Retain Rust, Tokio, and embedded Obscura where it gives a measured advantage. Put it behind the same backend interface as the browser relay and official APIs. Do not make correctness or account access depend on stealth/fingerprint spoofing.

- Introduce a small page-driver abstraction for the operations adapters actually need (find/query, click, type, evaluate, response observation, optional thread metadata). Keep the core independent of Obscura-specific types where practical.
- Fix invalid/non-portable selector assumptions; keep provider-specific selectors scoped to the composer and enabled controls.
- Replace silence-only completion heuristics with the strongest supported completion signal. Investigate a supported network/event stream interface; retain DOM observation as a tested fallback. Do not depend on undocumented provider protocols without versioned fixtures and graceful fallback.
- Handle identical consecutive answers by tracking response-node identity/count, not text difference alone.
- Ensure every queue/lock/page failure returns a typed error; implement cancellation and bounded queue timeouts.
- Pin the Obscura dependency to a reviewed revision and run clean builds, security audits, and adapter contract tests on every upgrade.
- Measure real providers: startup/warm-up time, memory per page, queue time, time-to-first-token, inter-delta latency, total completion time, failure rate, and CPU. Compare Obscura and real-browser relay on the same prompts and network conditions.
- Add concurrency only after measuring page memory and provider behavior. A single page per provider means serialized work; use a bounded per-provider page pool only if needed.

## 8. Security and operations baseline

- Bind to loopback by default; add hashed bearer API keys, per-key provider allowlists, quotas/rate limits, and separate administrative permissions.
- Validate configuration at startup; fail fast on unknown providers or unsafe combinations.
- Encrypt sensitive state, set restrictive file permissions, support key rotation, and keep runtime data in a documented writable directory.
- Add graceful shutdown and flush persistent state safely.
- Correct the Docker build context for vendored dependencies, use a non-root user, configure a writable data volume, and add a real health check.
- Add structured request-level logs with request ID, backend/provider, queue duration, TTFT, total duration, and outcome—without message content or secrets.
- Add metrics for request counts, latency histograms, queue depth, backend/session state, and adapter failures.
- Use `cargo fmt --check`, `cargo clippy -- -D warnings` where practical, `cargo test`, `cargo audit`, dependency policy checks, and a clean Docker build in CI.

## 9. Testing strategy and exit gates

1. **Unit tests:** message reconciliation/idempotency, memory windowing, summaries, exact routing, fallback policy, session-state transitions, domain allowlists.
2. **API conformance tests:** JSON and SSE shape, usage/finish events, malformed parameters, timeouts, cancellation, and error mapping against a fake backend.
3. **Memory integration tests:** restart persistence, delete/retention, tenant isolation, and provider switch with full context.
4. **Adapter fixtures:** recorded/sanitized HTML or event fixtures per provider; test composer detection, send behavior, response extraction, completion detection, and identical-answer handling offline.
5. **Relay security tests:** invalid pairing, expired/replayed tokens, hostile Origin, remote bind refusal, disallowed navigation, secret redaction, unlink/revocation.
6. **Live canary:** opt-in and rate-limited, never required for every CI run; emits machine-readable results and stops after repeated challenges.
7. **Benchmarks:** reproducible baseline before claiming latency, memory, or throughput improvements.

## 10. Phased roadmap

| Phase | Scope | Exit gate |
| --- | --- | --- |
| 0 — Secure and stabilize | Loopback bind, auth, protect login/relay routes, remove secret logging, fix duplicate turn writes and discarded context, fix clean Docker build | Security tests and baseline CI pass |
| 1 — Canonical API | Request/event/error types, backend trait, exact routing, parameter validation, SSE conformance, cancellation, explicit fallback policy | Fake-backend conformance suite passes |
| 2 — Durable memory | SQLite schema, idempotent log, context assembly, summaries, provider-thread map, tenant scoping, retention/delete APIs | Restart test passes; provider switch preserves context |
| 3 — Browser relay MVP | Local extension pairing, user-driven login, authenticated relay, session-state machine, revocation | Two providers can be linked and unlinked safely; no password enters gateway |
| 4 — Obscura hardening | Page abstraction, adapter fixes, response completion, fixtures, dependency pinning, performance baseline | Adapter fixtures pass and benchmark report is published |
| 5 — Optional capabilities | Official API adapters, long-term memory, multi-account support, bounded page pools | Each feature has security review and dedicated exit tests |

## 11. Decisions and non-goals

Recommended defaults:

- Start as a **single-user local gateway**; design tenant IDs into storage but do not expose multi-user access until isolation is tested.
- Make official provider APIs the preferred backend where available and appropriate.
- Keep long-term memory off by default; conversation history and durable personal memory are distinct features.
- Prefer real-browser relay over reusable cookie export.
- Keep Obscura as an optional performance backend, with correctness and account access never depending on anti-detection behavior.
- Do not promise universal provider support: each provider must pass its own compatibility, terms, security, and reliability gates.

## 12. Implementation checklist

- [ ] Verify each finding against current source and record tests run.
- [ ] Add secure defaults and gateway authentication.
- [ ] Define canonical chat request/event/error types and backend trait.
- [ ] Fix memory append idempotency and context injection.
- [ ] Add SQLite persistence and provider-thread mapping.
- [ ] Implement API conformance and memory integration tests.
- [ ] Design and threat-model local browser pairing before coding the extension.
- [ ] Implement relay revocation and session state tracking.
- [ ] Harden Obscura adapters and pin dependencies.
- [ ] Add CI, metrics, health/readiness, Docker verification, and benchmarks.
