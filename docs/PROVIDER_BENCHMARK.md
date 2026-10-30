# Provider benchmark harness

Run from the repository root:

```sh
python3 scripts/provider_bench.py --mode fake --iterations 20
```

Fake mode exercises repeatable measurement/reporting without credentials or network.
Its TTFT and switch values are synthetic control timings, **not** provider or browser
performance claims.

For an OpenAI-compatible endpoint:

```sh
export CHAT_GATEWAY_BENCH_ENDPOINT=http://127.0.0.1:8080/v1
export CHAT_GATEWAY_BENCH_API_KEY=...
export CHAT_GATEWAY_BENCH_MODEL=...
python3 scripts/provider_bench.py --mode live --iterations 10
```

Live mode measures request-to-first-response-byte (TTFT), including gateway,
network, and upstream latency. It does not measure browser-page RSS or true
provider switching; the process RSS fields describe only this benchmark process.
Save the JSON output with host, OS, gateway revision, endpoint/provider, and
model details when publishing results. Never commit credentials or raw prompts.

The harness intentionally labels unsupported measurements rather than presenting
synthetic values as real provider data. Extend it with a provider control-plane
switch measurement only when the gateway exposes a stable, authenticated switch
operation.
