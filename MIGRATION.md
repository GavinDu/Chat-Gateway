
# Migration from chromiumoxide to Obscura Native

## Performance

The figures previously listed here were design targets, not measurements from
this repository, and are intentionally not presented as verified results. The
SQLite memory microbenchmark can be run with `cargo bench --locked --bench
memory_sqlite`; it measures memory-layer operations only, not browser startup,
per-provider resident memory, provider switching, or provider TTFT.

A reproducible provider benchmark harness is available in
[docs/PROVIDER_BENCHMARK.md](docs/PROVIDER_BENCHMARK.md). Fake mode validates the
measurement/reporting path without claiming real provider performance; live mode
measures request-to-first-byte latency against an OpenAI-compatible endpoint.

## Changes
1. Removed chromiumoxide
2. Embedded obscura-core with render,stealth features
3. ObscuraPool with pre-warmed pages
4. Session restore now saves cookies + localStorage
5. Qwen selectors i18n robust

## Testing

Run the live adapter diagnostic for Qwen with the default `config.yaml`:

```bash
cargo run -- --test-adapter qwen
```

This diagnostic uses the configured provider and may require a reachable provider
site and a valid session. It is not an offline test; use `cargo test --locked` for
the offline Rust test suite.

## Current runtime and verification

The current example configuration keeps all browser-backed providers disabled by
default. Before enabling one, review [the provider policy](docs/PROVIDERS_POLICY.md)
and enable only providers whose applicable terms permit the intended use. A
valid configuration with no enabled providers is intentional and does not
silently opt in to browser automation.

For a local live diagnostic, configure and authenticate the provider first:

```bash
cargo run -- --config config.yaml --test-adapter qwen
```

To run diagnostics for every enabled provider and emit one JSON record suitable
for automation logs:

```bash
cargo run -- --config config.yaml --all --json
```

Live diagnostics need provider-site access and a valid session; they are not a
substitute for offline CI tests. The required offline checks are:

```bash
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo test --locked
cargo bench --locked --bench memory_sqlite
cargo run --locked --example provider_bench -- --fake --smoke
docker build --pull --tag chat-gateway-rs:ci .
docker run --rm --entrypoint /usr/local/bin/chat-gateway-rs chat-gateway-rs:ci --help
```

The Docker image binds to loopback inside its network namespace. Do not publish
its port to an untrusted network; use the deployment guidance and preserve the
loopback-only security boundary.
