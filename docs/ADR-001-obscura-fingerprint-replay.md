# ADR: Obscura fingerprint replay limitations

- **Status:** Accepted as a documented limitation; revisit after the browser relay can export a supported fingerprint profile
- **Date:** 2026-10-28
- **Related issue:** #132

## Decision

Do not claim that a restored provider cookie reproduces the browser identity that created it. The current gateway does not have a supported end-to-end mechanism to import and apply a linked browser's complete fingerprint. Keep the embedded Obscura profile stable, and do not enable random profile rotation as a workaround for session bounces.

## Findings at the pinned dependency revision

Chat-Gateway pins `obscura-core` to `09790944a405ba81a7df27bc2577c6b0db283900`.

- At that revision, Obscura's `BrowserProfile` contains a user-agent string, platform, UA platform name, and UA platform version. Its profile selector defaults to the first profile; `OBSCURA_PROFILE=<index>` can pin a profile and `OBSCURA_ROTATE_PROFILE=1` selects a random profile per context.
- Obscura's `BrowserContext` APIs accept an optional user-agent override, but Chat-Gateway's `build_browser` currently configures only stealth and the storage directory. It does not supply a user-specific user agent or profile.
- The pinned profile schema does not expose a timezone field, and the pinned revision did not provide a documented `OBSCURA_TIMEZONE` setting. A newer upstream revision may have different capabilities; do not assume those capabilities exist in the pinned dependency.
- Matching the user-agent alone would not establish a faithful fingerprint. Platform/UA-CH values, timezone, locale, WebGL/canvas behavior, TLS characteristics, and egress IP can also differ. This ADR does not recommend spoofing all of those independently because inconsistent values can make the session less stable.

## Operational constraint

For a provider session linked in a browser, keep the gateway and the browser relay on the same host and network egress where practical. A stable egress IP reduces one source of mismatch but is not proof that the provider accepts the restored session. If a provider redirects to login or presents a challenge, use the existing re-login path rather than repeatedly changing profiles or retrying.

## What remains unverified

No live-provider experiment was performed as part of this repository-only investigation. In particular, this ADR does not claim that matching UA, locale, or timezone prevents a bounce for ChatGPT, Claude, Gemini, or any other provider.

## Follow-up design

1. Have the relay explicitly export only supported, non-secret profile metadata with user consent.
2. Add a typed profile contract to the gateway and Obscura integration; avoid reading arbitrary environment variables as a per-user profile API.
3. Support a profile only when the pinned Obscura API can apply it consistently across browser JavaScript and network requests.
4. Add deterministic fixture tests for profile selection and document the unsupported fields.
5. Run provider-specific experiments with test accounts and record results by provider, profile, and egress. Do not infer success from a single warm session.

Until that contract exists, fingerprint replay is a documented limitation rather than an implemented capability.
