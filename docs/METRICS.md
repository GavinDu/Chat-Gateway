# Gateway metrics and request logs

The gateway exposes Prometheus text metrics at `/metrics`. When API-key
authentication is enabled, this endpoint requires a key with the `admin`
scope. Keep the listener loopback-only; metrics are operational diagnostics,
not a public endpoint.

The `gateway_provider_session_state` gauge emits four one-hot samples per configured provider, with the bounded `state` values `unknown`, `ready`, `needs_login`, and `degraded`. A value of `1` marks the current state and the other two values are `0`. Provider names are configured labels; never add conversation IDs, URLs, or session error details to these labels. `unknown` means no recognized state is currently recorded, not that a live health probe succeeded.

## Interpreting latency

Use the bounded provider and outcome labels to compare request volume and
handler latency. Queue wait measures time waiting for the provider slot;
time to first token (TTFT) measures the delay until the first response content;
provider work measures the browser/provider portion; total chat duration
includes the request lifecycle. Gateway overhead is an estimate derived from
the measured intervals, not a separate upstream measurement.

Histogram buckets are intended for aggregate latency analysis. Do not infer
per-conversation traces from these metrics: conversation identifiers,
prompts, response bodies, API keys, and authorization headers must never be
metric labels or structured-log fields.

## Request completion logs

Each completed HTTP request emits a structured log entry containing the
request ID, HTTP method, normalized route, status code, and elapsed duration.
Provider attempts also emit a structured completion entry with the same request
ID, provider name, queue wait, TTFT (when available), provider-work duration,
total chat duration, and success/error outcome. These entries deliberately omit
prompt text, response bodies, conversation IDs, API keys, and session details.
Use the request ID to correlate a client report with its provider timing. Avoid
enabling debug logging in production without reviewing the provider/session
privacy implications.

## Operational checks

- Scrape `/metrics` only from a trusted local monitoring process.
- Confirm admin-scope enforcement when authentication is enabled.
- Alert on elevated provider failures and queue/handler latency rather than
  individual prompt content.
- Treat gateway-overhead values as estimates and compare them with the
  queue, TTFT, provider-work, and total-duration measurements together.

Relay session imports expose `gateway_relay_session_import_phase_ms` with fixed
`phase` values `queue`, `persist`, `activate`, `total`, and `activation_tail`.
The activation tail measures work still holding the browser gate after a timed-out
HTTP import response; do not add it to HTTP response latency. These metrics carry
no provider names, cookie values, session identifiers or URLs.

`gateway_provider_timeout_total` separates `submission`, `first_content`, and
`generation` timeouts. The first-content phase follows acknowledged submission but
has no fresh assistant content; generation means partial content arrived before
the shared deadline expired. None is evidence of a successful answer.

## Performance measurement contract

See [the telemetry design and metric catalog](PERFORMANCE_TELEMETRY.md) for phase
boundaries, client/server differences, instrumentation coverage and the experiment
protocol. HTTP handler time stops when the response object is returned; it is not
full SSE completion time. Existing histograms keep millisecond units and now have
finite buckets through 300 seconds so Meta AI's long tail is observable.

Additional measurements:

- `gateway_chat_input_requests_total{kind,transport}` counts validated API inputs.
  Kind is a bounded, untrusted client hint from `X-Chat-Gateway-Request-Kind`;
  `primary`, `title`, `compaction`, `generate`, `canary`, or `unspecified`.
- `gateway_request_ttft_ms{provider,kind,transport}` measures request start to first
  backend content availability, including pacing and queues. It does not measure
  the browser UI rendering that content or a JSON client's full response.
- `gateway_chat_phase_duration_ms{provider,phase}` separates `pacing`, `slot`,
  `provider_context`, `backend_admission` and `persistence`. Phases can nest:
  backend admission is inside provider work. Returned errors are observed too.
- `gateway_provider_completion_decisions_total{provider,reason}` distinguishes
  `idle_grace` from `explicit_signal`. A browser decision is not proof of durable
  API success; use the existing canonical provider outcome counter as well.

`gateway_overhead_ms` includes pacing and other residual work; it is not a CPU
measurement. Request IDs remain in logs, never metric labels. Benchmark reports
retain failures, completion validation and sample counts rather than reporting
small-sample percentile estimates as service-level guarantees.
