# Epic #807 — merged-main provider acceptance matrix

This is the **live acceptance evidence register**, not a declaration that the
gateway is ready. Coding-only PRs and GitHub Actions can validate Rust logic,
Docker builds and synthetic DOM fixture contracts; they **cannot** demonstrate
working provider authentication, a real response or correct recovery from
browser-provider JavaScript HTTP 403. Live acceptance is delegated outside the
coding-only workflow and must use the **merged `main` build**, never a feature branch.

## Reproducibility and evidence envelope

Fill these fields for *each* live acceptance run; do not copy secrets into
GitHub issues, artifacts or logs:

- Main commit SHA: **not captured — live acceptance not run in this session**
- GitHub Actions run URL and all four job conclusions: **not captured for live run**
- Runtime environment: **not recorded** (OS, architecture, Docker image digest,
  Obscura/browser build, gateway config revision, network category)
- Session source and age: **not recorded** (e.g. extension-linked, embedded login;
  never record cookie contents, personal identifiers or vault paths)
- Evidence timestamp in UTC: **not recorded**
- Sanitized probe evidence: **none** (status, completion, HTTP class, first-token
  latency, finish reason, total duration, timeout phase only)

## Provider matrix

All nine providers have checked-in **synthetic** logged-out, idle, streaming and
done fixtures. "Not run" means there is **no live acceptance evidence** in
this coding-only review. These columns must only change with reproducible
results from a successful or failed merged-main execution.

| Provider | Synthetic fixtures | Login & refresh | Submit acknowledged | First response / terminal done | Restart recovery | Tracking |
| --- | --- | --- | --- | --- | --- | --- |
| ChatGPT | Yes (synthetic) | Not run | Not run | Not run | Not run | P0 #809, #811 |
| Claude | Yes (synthetic) | Not run | Not run | Not run | Not run | #814 |
| Gemini | Yes (synthetic) | Not run | Not run | Not run | Not run | P0 #811 |
| Grok | Yes (synthetic) | Not run | Not run | Not run | Not run | P0 #809, #811 |
| Mistral | Yes (synthetic) | Not run | Not run | Not run | Not run | P0 #810 |
| Meta AI | Yes (synthetic) | Not run | Not run | Not run | Not run | #814 |
| Perplexity | Yes (synthetic) | Not run | Not run | Not run | Not run | #814 |
| Qwen | Yes (synthetic) | Not run | Not run | Not run | Not run | #814 |
| Manus | Yes (synthetic) | Not run | Not run | Not run | Not run | #814 |

## Live acceptance criteria (delegated separately)

1. Confirm the exact merged-main SHA and all four Rust CI jobs: `test`,
   `embedded-native`, `docker-build` and `docker-arm64-low-memory`.
   Record the environment and browser-session source without storing secrets.
2. For each of the nine providers, distinguish saved cookies from a verified
   authenticated composer. Check login expiry, an unloaded page and challenge
   handling without attempting to bypass a provider challenge.
3. Send a short synthetic prompt through the actual gateway, verify a visible
   submission transition, first non-empty response content, a terminal
   successful completion and preservation of context when applicable.
4. Validate logout/relink, recovery after gateway/browser restart and provider
   switch. For Mistral specifically, classify observed script HTTP 403 or
   challenge evidence without implying that a selector override fixes blocked
   assets. Distinguish submission, generation and queue timeouts.
5. Mark each cell **pass**, **fail** (with linked sanitized evidence), or
   **not run**. Do not mark the Epic complete until every required scenario
   has grounded results. Synthetic HTML fixture CI remains complementary.

See [adapter diagnostics](ADAPTER_CANARY.md),
[live multi-provider canary](LIVE_PROVIDER_CANARY.md), and
[release testing](TESTING_CHECKLIST.md) for supported commands and setup.
