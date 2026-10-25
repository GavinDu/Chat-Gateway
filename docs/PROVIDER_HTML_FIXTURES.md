# Provider HTML fixture contracts

Run the offline fixture checks with:

```sh
cargo test --locked --test provider_fixtures
```

The checked-in files are **synthetic, sanitized contract fixtures**, not claims of fresh captures from provider websites. They cover the logged-out, idle, streaming, and completed-response shapes for all eight adapters and are intentionally free of account data. They are useful for deterministic contract and scrubbing checks, but do not prove that a live provider's current DOM matches the adapter selectors.

When refreshing from an authorized test session, capture only a minimal DOM snapshot, redact before writing it to disk, and run the scrubbing test before committing. Never include cookies, local storage, authorization headers, tokens, account identifiers, or email addresses. Live adapter canaries remain a separate, opt-in check because they require valid provider sessions.
