# Session fingerprint restoration: current limitation

## Current behavior

The gateway restores provider-scoped cookies and local-storage snapshots before warming a provider page. The persisted session format does **not** currently capture or replay the source browser's user-agent, language preferences, timezone, or other browser fingerprint attributes. Session restoration therefore means restoring the allowed web-storage state, not cloning the source browser environment.

Provider sessions may still redirect to login or trigger additional verification after restoration. A matching egress IP and compatible browser fingerprint may matter to some providers, but this repository has not established that experimentally for each supported provider.

## Operational guidance

- Run the gateway from the same trusted machine/network egress as the linked browser when practical.
- Treat a redirect-to-login probe as an expired session and relink the provider; do not repeatedly retry it as a transient failure.
- Do not log cookies, local-storage values, or raw session snapshots while investigating login redirects.
- The configured `obscura.user_agent` override uses the pinned engine’s public API. Locale and timezone replay remain unsupported; session snapshots do not capture or automatically replay a source browser fingerprint.

## Validation plan

For each provider, a controlled follow-up experiment should compare session restoration under:
1. the default gateway configuration,
2. an explicitly configured user-agent override where appropriate,
3. the same network egress where feasible.

Record only the outcome (authenticated page, login redirect, or challenge), provider, engine revision, and configuration. Never include credentials or session contents.

## Status

This is a documentation of the current integration boundary, **not** evidence that fingerprint matching fixes any provider. The provider-by-provider experiment and an implementation using a supported engine API remain open work for issue #132.
