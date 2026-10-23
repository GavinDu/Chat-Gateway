# Chapter 5 — Provider Adapters

## 5.1 Production scope

Production adapters are:

| Provider | Config name | Default alias |
|---|---|---|
| Qwen | `qwen` | `qwen` |
| Meta AI | `metaai` | `meta-ai` |

All require a manually captured browser session. The router can fall back among enabled
providers; model aliases remain configurable.

## 5.2 Adapter contract

Each adapter identifies itself, exposes configured aliases, checks its session state,
and sends a message through its browser tab. Shared utilities locate fallback selectors,
submit input, poll the newest response across SPA navigation, and stream output to the
API layer. Current response polling evaluates the provider DOM every 200 ms and treats
four seconds without a text change as completion.

Qwen uses a reusable provider browser plus a configurable warm-tab pool (default two).
Meta AI reuses its provider browser but opens fresh tabs for new conversations; Qwen's
warm-tab lifecycle was tested on Meta and rejected because it disrupted reliable
requests. Both adapters retain conversation tabs and use bounded native DOM input/send
events rather than animation-sensitive visual-stability clicks on their hot paths.

## 5.3 Selector status

Selectors are implementation details that can change with provider UI releases.
`--test-adapter <provider>` is the supported live check. Provider selectors are present in
the adapter and were live-verified with authenticated sessions on 2026-09-05:

- Qwen: `textarea` input, an aria-labelled Send button, and current-answer polling.
- Meta AI: a Lexical `contenteditable` input, aria-labelled Send button, and
  `[data-testid="assistant-message"]` response content.

These observations are not permanent compatibility guarantees. A provider UI release
can invalidate them, and coding responses with significant whitespace should be included
in adapter tests because rendered browser text can differ from model source formatting.

## 5.4 Adding providers

Additional providers are not production-supported. A future adapter needs a manual-login
path, session handling, selector validation, routing registration, configuration entry,
and explicit verification before it can be documented as supported.