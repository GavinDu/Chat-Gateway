# Chapter 1 — Architecture

## 1.1 Overview

Go Proxy exposes a small OpenAI-compatible HTTP API over manually authenticated web
sessions for Qwen and Meta AI. Browser automation remains **Go + Rod over CDP**.

```text
client → HTTP validation/history → model router → provider adapter
                                             → browser/tab → provider web UI
                                             ← DOM response polling
       ← OpenAI JSON or SSE ← response collection/fallback decision
```

The router resolves a model alias, orders enabled providers by priority, and falls back
after a provider failure if no response bytes have been committed. Conversation IDs
retain a provider tab between turns. Each provider currently serializes requests across
its browser entry; Qwen can allocate a prewarmed tab for a new conversation, while
Meta AI reuses its browser process but opens a fresh tab.

Chapter 12 expands this into the complete request path and documents the performance
implications of browser locks, tab lifecycle, selector waits, response polling, health
checks, and fallback.

## 1.2 Components

- `main.go` loads configuration and supports server, `--login`, and `--test-adapter`
  modes.
- `internal/api` provides `/health`, `/v1/models`, and `/v1/chat/completions`.
- `internal/router` selects providers and tracks degraded state.
- `internal/providers` contains the Qwen and Meta AI adapters.
- `internal/browser` launches Chromium through Rod, restores/captures sessions, and
  manages conversation tabs.
- `internal/dashboard` exposes status, config editing, request activity, and a
  server-controlled remote view of isolated manual-login browsers.

## 1.3 Session boundary

Authentication is user-driven: launch `--login <provider>` or use the dashboard login
action, finish login in the temporary browser, then save the session. The dashboard
receives short-lived screenshots and sends validated input events; it never exposes
the CDP endpoint, browser DOM, cookies, or localStorage. The proxy persists cookies
and localStorage in `sessions/<provider>.json`; an optional
`chrome_profile_dir` persists provider browser profiles. It never collects, saves, or
uses raw account passwords.

## 1.4 Deferred work

Playwright is not part of the current implementation; a possible migration is tracked
in the backlog. CEF was evaluated and rejected; see the historical report in Chapter 10.