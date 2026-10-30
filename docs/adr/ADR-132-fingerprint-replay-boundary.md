# ADR: Browser session fingerprint replay boundary

**Status:** Accepted limitation; implementation deferred pending an upstream capability
**Issue:** #132
**Date:** 2026-10-30
**Scope:** Embedded Obscura browser sessions in Chat-Gateway

## Decision

Chat-Gateway does not claim to faithfully replay the source browser's
user-agent, language, or timezone. The current session handoff restores
provider cookies and local storage; it does not persist or apply those
fingerprint values as part of the provider session contract.

The gateway must not synthesize or spoof fingerprint values to evade provider
security controls. If a provider requires continuity with the original browser
fingerprint, use the same machine and network egress for the linked browser and
gateway where practical, and treat this as an operational constraint rather
than a guarantee. A changed egress IP can independently invalidate cookies or
trigger provider verification.

## Findings and limits

- The current gateway session schema and restore flow are cookie/local-storage
  oriented; there is no gateway-owned, persisted UA/language/timezone tuple
  associated with a provider session.
- The current integration does not expose a reviewed, provider-independent
  API for setting all three values on a restored Obscura page.
- Obscura stealth behavior may choose browser characteristics internally.
  Matching a user-agent string alone would not prove matching the complete
  fingerprint (client hints, platform, fonts, rendering, WebGL, and other
  signals may differ).
- No live provider experiment is claimed by this ADR. Production credentials
  and sensitive prompts must not be used for fingerprint experiments.

These findings describe the gateway integration surface, not a guarantee that
the pinned Obscura dependency can never expose such controls. Revisit only
after reviewing the exact pinned upstream API and documenting the supported
fields and their lifecycle.

## Safe experiment plan

1. Inspect the exact Obscura revision pinned in Cargo.toml and record the
   specific public APIs, if any, for user-agent, locale/language, and timezone.
2. Add a typed session-fingerprint structure only if all required fields can be
   set through supported APIs before navigation and remain stable for the
   page/session lifetime.
3. Use a dedicated test account and provider-approved test workflow. Do not
   bypass CAPTCHAs, access controls, rate limits, or anti-bot challenges.
4. Compare behavior with and without matching values while keeping the
   machine and egress IP fixed; record provider, date, region, and outcome.
5. If the dependency has no supported override, retain this ADR as the
   documented limitation and do not inject JavaScript patches or attempt to
   defeat fingerprinting controls.

## Consequences

- Cookie/local-storage restoration remains the supported session handoff.
- Session continuity is not guaranteed across machines, browser environments,
  or network egress changes.
- Providers may require re-authentication or additional verification.
- The issue can be considered documented at the integration boundary; any
  future implementation requires new evidence from the pinned upstream API
  and an explicitly approved, policy-compliant experiment.
