# Chapter 2 — Configuration

## 2.1 `config.yaml`

```yaml
server:
  port: 8080
  headless: true
  chrome_profile_dir: ""

providers:
  - name: qwen
    enabled: true
    priority: 1
    timeout_seconds: 120
    session_pool_size: 2
    models: [qwen]
  - name: metaai
    enabled: true
    priority: 2
    timeout_seconds: 120
    models: [meta-ai, llama]
  - name: claude
    enabled: true
    priority: 3
    timeout_seconds: 120
    models: [claude, claude-sonnet, claude-opus, claude-haiku]
  - name: chatgpt
    enabled: true
    priority: 4
    timeout_seconds: 120
    models: [gpt, chatgpt, gpt-5, gpt-4o]
  - name: gemini
    enabled: true
    priority: 5
    timeout_seconds: 120
    models: [gemini, gemini-pro, gemini-flash]
```

Production provider names are `qwen`, `metaai`, `claude`, `chatgpt`, and
`gemini`. `enabled` excludes an
adapter, `priority` controls fallback order (lower first), and `models` defines
case-insensitive aliases. `headless` may be overridden per provider.
`session_pool_size` controls Qwen's authenticated warm-tab capacity and defaults to
two. It is not applied to Meta AI because Meta uses fresh tabs for reliable new
conversations.

`chrome_profile_dir`, when set, stores a separate persistent profile below
`<chrome_profile_dir>/<provider>`. Session capture otherwise writes cookies and
localStorage to `sessions/<provider>.json`.

## 2.2 Runtime options

`PORT` overrides `server.port`. `dashboard_secret`, if configured, protects dashboard
mutation routes and every remote-login status, frame, and input route. Configuration
changes that affect routing priority take effect after a restart; session-file changes
are reloaded by the browser pool.

`timeout_seconds` is the provider attempt budget, not a latency target. A slow failed
attempt can consume that budget before fallback starts. Performance measurement and
deadline improvements are covered in Chapter 12.

## 2.3 CLI

```text
./go-proxy --config config.yaml
./go-proxy --login qwen --config config.yaml
./go-proxy --test-adapter qwen --config config.yaml
```

`--login` always opens a headed browser for manual authentication and saves no raw
passwords. `--test-adapter` is a live diagnostic that may send a test prompt.