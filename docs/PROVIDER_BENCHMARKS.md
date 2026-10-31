# Provider benchmark protocol

This protocol separates repeatable offline memory measurements from measurements
that require a live, authenticated browser-backed provider. Do not compare live
runs unless the machine, provider, session state, configuration, and warm/cold
start conditions are recorded.

## 1. Offline memory baseline

Use the repository's isolated SQLite microbenchmark:

```sh
cargo bench --locked --bench memory_sqlite
```

For a short smoke run:

```sh
cargo bench --locked --bench memory_sqlite -- --smoke
```

The benchmark creates a unique temporary directory and database. It reports
sample count, average, p50, p95, and throughput for SQLite turn writes, context
construction, provider/tab lookups, and conversation deletion. These values
measure the memory layer only; they are not provider TTFT or browser startup
measurements.

## 2. Live provider measurements

Only run against providers that are enabled intentionally, whose terms permit
the intended automation, and for which a valid session has been established.
Do not include cookies, tokens, prompts, or response bodies in benchmark output.

1. Record the git commit, OS/CPU/RAM, Rust version, provider ID, configuration
   (with secrets removed), and whether the browser worker and provider session
   were warm or cold.
2. Start the gateway and confirm `/ready` succeeds. Record process RSS after
   warm-up and after a fixed idle interval.
3. Send a fixed, non-sensitive prompt using the same model and output limit for
   each sample. Record request start, first streamed token, and completion time.
   Report TTFT and total latency as p50/p95, with sample count and failures.
4. Measure provider switching separately: alternate between two deliberately
   enabled providers using the same prompt and report the first request after
   each switch separately from steady-state requests.
5. Repeat the run at least three times and report each run; do not pool results
   across different machines, provider session states, or configuration.
6. Stop the gateway cleanly and preserve only aggregate measurements.

The loopback `/metrics` endpoint exposes request timing, provider queue wait,
TTFT, provider work, total chat duration, and estimated gateway overhead. When
available, use these counters to cross-check client-side timing. The metrics
endpoint is loopback-only and may require an admin-scoped API key when key
authentication is enabled.

### Reproducible concurrency sweep

Run the opt-in live harness against one deliberately enabled, authenticated
provider. It sends independent conversations at concurrency 1, 2 and 4 by
default, measures end-to-end TTFT from request submission, total latency,
failures, and optional before/after gateway RSS:

```sh
CHAT_GATEWAY_API_KEY=<local-api-key> \
python3 scripts/provider_concurrency_benchmark.py \
  --provider chatgpt --model <provider-model> --samples 8 --pid <gateway-pid>
```

Use a non-sensitive test account and prompt. The harness validates the
gateway's provider identity header and SSE terminal event. Output contains
aggregate timing/failure counts only; it does not print prompts, response text,
cookies or credentials. RSS values are snapshots before and after the sweep,
not peak RSS. Run at least three times per machine/session state and keep
provider-specific results separate. A nonzero exit indicates one or more
failed requests; do not silently discard failures when comparing latency.

### Provider-switch latency sweep

Measure first-request latency after changing providers separately from a
same-provider follow-up. Supply an ordered sequence with explicit provider/model
pairs. Each provider retains its own conversation ID within a repetition, so
returning to a provider measures a follow-up on its existing gateway thread.

\`\`\`sh
CHAT_GATEWAY_API_KEY=<local-api-key> \
python3 scripts/provider_switch_benchmark.py \
  --step chatgpt=<chatgpt-model> \
  --step claude=<claude-model> \
  --step chatgpt=<chatgpt-model> \
  --step gemini=<gemini-model> \
  --repeat 3
\`\`\`

The report separates initial provider requests, first requests after a provider
switch, and same-provider follow-ups. It includes p50/p95 TTFT and total latency
plus failure counts. It never prints prompts, response text, credentials or
conversation IDs. Run only against deliberately enabled, authenticated
providers; compare like-for-like model and session conditions. Failed requests
make the command exit nonzero and are not silently dropped from the failure
report.

## Reporting template

| Field | Value |
|---|---|
| Commit / run date | |
| Machine / OS / Rust version | |
| Provider and model | |
| Warm or cold browser/session | |
| Sample count / failed requests | |
| TTFT p50 / p95 | |
| Total latency p50 / p95 | |
| Provider-switch first-request p50 / p95 | |
| RSS after warm-up / idle | |
| Notes / deviations | |

Do not publish a result table with empty measurements represented as zero. Mark
unmeasured fields as unavailable and keep live-provider results separate from
the offline SQLite baseline.
