# Spike 0002: Provider response network tap

- **Status:** Feasibility confirmed; implementation plan proposed
- **Date:** 2026-10-25
- **Related issue:** [#137](https://github.com/GavinDu/Chat-Gateway/issues/137)
- **Pinned engine revision:** `09790944a405ba81a7df27bc2577c6b0db283900`

## Finding

The pinned Obscura revision documents a Rust-library API with:

- passive `Page::on_request` and `Page::on_response` callbacks, with stable callback IDs that can be detached;
- response metadata including URL and resource type, plus a response body;
- `Page::add_preload_script`, which runs before page scripts;
- `Page::enable_interception` for active request interception.

The upstream Rust-library guide at the pinned revision is available at
https://github.com/h4ckf0r0day/obscura/blob/09790944a405ba81a7df27bc2577c6b0db283900/docs/Use-as-a-Rust-library.md.

This confirms that a network-observation spike is feasible without adding Chromium/CDP as a separate process. It does **not** yet prove that passive response callbacks expose incremental chunks for long-lived SSE responses: the documented response body is buffered, and upstream notes distinguish early response-header delivery from incremental response streaming.

## Proposed implementation

1. Add an opt-in per-adapter network-tap capability. Register observers before sending a prompt and detach them when the send completes or is cancelled.
2. Initially observe metadata only: URL, resource type, status/content type if available, and bounded body size. Do not log request/response bodies, cookies, authorization headers, prompt text, or provider session data.
3. Prototype two providers behind a feature flag. Select the providers only after a sanitized local trace confirms a stable response endpoint and format.
4. Prefer passive callbacks first. Use a preload hook only if passive callbacks cannot expose stream events; never block or rewrite provider requests for ordinary generation.
5. Keep DOM polling as the fallback when no supported response event arrives, the payload is unrecognized, or a provider changes its protocol.
6. Contract-test parsing against sanitized recorded fixtures; tests must not contain real sessions, tokens, user prompts, or personal data.

## Acceptance tests

- The tap does not change request behavior and is detached after success, failure, timeout, and cancellation.
- A recognized stream yields ordered content deltas and one terminal completion signal.
- Unknown endpoints and malformed frames fall back to DOM polling without failing a request.
- Diagnostics expose only provider name, event type, timing, and bounded byte counts.
- Fixture tests cover chunk boundaries, malformed frames, empty frames, cancellation, and duplicate terminal events.

## Prototype status (2026-10-30)

An opt-in metadata-only observer is implemented for warmed ChatGPT and Claude pages. Set `CHAT_GATEWAY_PROVIDER_NETWORK_TAP=1` to enable it. Debug events contain only provider, response host, resource type, HTTP status, and bounded response byte count. URL paths and query strings, headers, cookies, prompts, and response bodies are never logged.

The pinned passive callback exposes a complete buffered response, not proven incremental SSE chunks or a terminal signal. Existing DOM polling therefore remains authoritative. A sanitized trace from real provider sessions is still required before selecting stable endpoints or implementing a stream parser.

## Decision

Proceed with the prototype, but keep it separate from the existing DOM-based completion path until the pinned API is compiled against directly and a real provider trace demonstrates incremental delivery. If only buffered bodies are available, use the tap for metadata/response correlation and retain DOM polling for streaming.
