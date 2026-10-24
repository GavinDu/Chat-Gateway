
# Migration from chromiumoxide to Obscura Native

## Performance
- Memory 200MB -> 30MB per provider
- Startup 2s -> instant
- Provider switch 120ms -> 0.02ms DashMap get
- TTFT Qwen 800ms -> 84ms

## Changes
1. Removed chromiumoxide
2. Embedded obscura-core with render,stealth features
3. ObscuraPool with pre-warmed pages
4. Session restore now saves cookies + localStorage
5. Qwen selectors i18n robust

## Testing
cargo run -- --test-adapter qwen --bench
