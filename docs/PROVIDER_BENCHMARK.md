# Provider benchmark harness

The repository currently contains two complementary probes. Neither can report
browser-page RSS or isolate provider-switch overhead until the gateway exposes
a stable per-provider measurement/control-plane interface. Do not interpret
synthetic timings as provider performance.

## Recommended gateway probe (Rust)

Run an offline smoke test:

```sh
cargo run --locked --example provider_bench -- --fake --smoke
```

For a live, explicitly configured gateway:

```sh
cargo run --locked --example provider_bench -- \
  --url http://127.0.0.1:8080/v1/chat/completions \
  --api-key "$CHAT_GATEWAY_API_KEY" \
  --model chatgpt/<configured-model> \
  --providers chatgpt,claude \
  --iterations 20 \
  --pid "$(pgrep -n chat-gateway-rs)"
```

Live mode sends requests to the supplied gateway URL and reports total request
time and time to the first non-empty streamed content delta. Pin the model to a
provider using `<provider>/<model>`; the example alternates the provider list.
`provider_switch_ttft_ms_median` is the TTFT of a request immediately after the
provider name changes. It includes provider startup/queue/network/upstream work
and is **not** an isolated switch-cost measurement. When `--pid` is supplied,
RSS is read for that process; this is gateway-process RSS, not per-page RSS.
Without `--pid`, RSS is `n/a`.

Fake mode starts a local synthetic SSE endpoint with a fixed delay. It validates
the request and measurement path only. Its fake gateway-overhead value is a
calculation against that configured delay, not a production estimate.

## JSON reporting probe (Python)

Run repeatable offline reporting:

```sh
python3 scripts/provider_bench.py --mode fake --iterations 20
```

For a live OpenAI-compatible endpoint:

```sh
export CHAT_GATEWAY_BENCH_ENDPOINT=http://127.0.0.1:8080/v1
export CHAT_GATEWAY_BENCH_API_KEY=...
export CHAT_GATEWAY_BENCH_MODEL=...
python3 scripts/provider_bench.py --mode live --iterations 10
```

The Python fake mode reports synthetic TTFT and switch timings. Python live mode
reports TTFT only; its RSS fields describe the Python benchmark process, not the
gateway or browser pages. The JSON includes a UTC timestamp, Python version,
platform identifier, sample count, median and p95. Add the gateway revision,
host/OS details, provider, model and endpoint class when publishing results.
Never commit credentials or raw prompts.

## Measurement contract and remaining work

| Measurement | Available today | Interpretation |
|---|---|---|
| TTFT | Rust live and Python live | End-to-end request-to-first-content latency |
| Total request time | Rust live | End-to-end request duration |
| Provider switch | Rust alternating-provider sequence; Python fake only | Rust value is next-provider TTFT, not isolated switch cost; Python is synthetic |
| RSS | Rust with `--pid`; Python own process | Process RSS only; not per-provider page RSS |
| Gateway overhead | Rust fake only | Synthetic delay subtraction; not a live decomposition |

For publishable measurements, record the exact gateway commit, host/OS/CPU,
provider, configured model, iteration count, warm/cold state, and whether the
run was fake or live. Keep raw credentials and prompts out of reports.

Issue #144 remains open for true per-provider page RSS, isolated provider-switch
latency, and measured results from a documented live environment. Do not fill
those gaps with synthetic numbers or infer them from process-level RSS.
