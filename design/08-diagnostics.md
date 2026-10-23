# Chapter 8 — Diagnostics

## Manual login

```bash
./go-proxy --login qwen --config config.yaml
```

Use `qwen`, `metaai`, `claude`, `chatgpt`, or `gemini`. The command opens a
headed browser, where the user signs
in directly, then captures cookies and localStorage when login is finished. It never
accepts or saves raw passwords. The dashboard provides the same manual workflow.

## Adapter check

```bash
./go-proxy --test-adapter qwen --config config.yaml
```

The check opens a headed session and tests the adapter's input, submission, and response
observation path. It is a live diagnostic and can send a test prompt; a passing result is
the only basis for claiming a selector set is verified. Qwen and Meta AI's current input,
Send, and response paths were live-verified with authenticated sessions on 2026-09-05;
provider UI changes can invalidate that result.

## Logs

Structured logs record browser, routing, provider, selector, and request events. Session
expiry or selector failures should be resolved by manually refreshing the affected
provider session or updating the adapter after verification.

## Performance diagnostics

For every latency investigation, verify `X-Provider` and the dashboard attempt history
before treating the response as a provider success. Record total duration, fallback,
new versus continued conversation, Qwen warm hit/miss when visible, and exact output
correctness. Run multiline or whitespace-sensitive code through a compiler or parser;
HTTP 200 and non-empty text do not prove response integrity. New provider selectors
must pass this live check after the provider session is captured.

Current logs expose total request and attempt information but not queue/page/selector/
first-output stage timings. Chapter 12 defines the missing telemetry and the repeatable
benchmark method.