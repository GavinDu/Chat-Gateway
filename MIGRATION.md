
# Migration from chromiumoxide to Obscura Native

## Performance

The figures previously listed here were design targets, not measurements from
this repository, and are intentionally not presented as verified results. The
SQLite memory microbenchmark can be run with `cargo bench --locked --bench
memory_sqlite`; it measures memory-layer operations only, not browser startup,
per-provider resident memory, provider switching, or provider TTFT.

## Changes
1. Removed chromiumoxide
2. Embedded obscura-core with render,stealth features
3. ObscuraPool with pre-warmed pages
4. Session restore now saves cookies + localStorage
5. Qwen selectors i18n robust

## Testing
cargo run -- --test-adapter qwen --bench
