# Gateway metrics and request logs

The gateway exposes Prometheus text metrics at `/metrics`. When API-key
authentication is enabled, this endpoint requires a key with the `admin`
scope. Keep the listener loopback-only; metrics are operational diagnostics,
not a public endpoint.

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

Each completed request emits one structured log entry containing the request
ID, HTTP method, normalized route, status code, and elapsed duration. Use the
request ID to correlate gateway logs with a client report. Avoid enabling
debug logging in production without reviewing the provider/session privacy
implications.

## Operational checks

- Scrape `/metrics` only from a trusted local monitoring process.
- Confirm admin-scope enforcement when authentication is enabled.
- Alert on elevated provider failures and queue/handler latency rather than
  individual prompt content.
- Treat gateway-overhead values as estimates and compare them with the
  queue, TTFT, provider-work, and total-duration measurements together.
