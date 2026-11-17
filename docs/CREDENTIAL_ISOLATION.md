# Isolate credential failures from Obscura failures

A saved snapshot proves persistence. A website reply, visible editor, cookie count,
WebSocket upgrade, HTTP 403, or timeout does not prove authenticated account access.

## Obtain a Chrome control without resyncing

1. In the existing signed-in Chrome profile, open https://chatgpt.com/.
2. Open DevTools Console and run the complete expression in
   [provider_credential_probe.js](../scripts/provider_credential_probe.js).
   Inspect the fulfilled result object. The probe only reads the first-party
   session endpoint; it does not sync, log out, send a chat, or modify storage.
3. Record only the returned diagnostic object and attempt time. Never export
   the authentication response, cookies, tokens, account details, or a HAR.

The ChatGPT probe requires a first-party session response containing an account
object, an access token, and a future expiry. Those values stay inside the page.
Only fixed categories and timing leave it. HTTP 401 and definitive empty session
responses mean unauthenticated. Blocks, timeouts, expired/partial evidence, and
unrecognized schemas remain unknown.

Meta AI deliberately returns account_verifier_not_established and makes no
request. A documented/source-grounded account check must be implemented before
an automated Meta AI credential-validity verdict is available. Do not invent
a GraphQL query or treat anonymous chat as account proof.

## Compare against the gateway

Use the same provider, account, stored snapshot generation, and a close time
window. Record the exact gateway source/image, import/activation result, and
request ID. Do not compare an installed gateway holding sessions against a
fresh sandbox gateway without sessions.

The recovery branch verifies ChatGPT via /api/auth/session inside the actual
Obscura provider page before composer use. Its relay status exposes short-lived
authentication proof separately from linked/saved state. Existing older installs
may only return unknown; that cannot diagnose credentials. CLI adapter diagnostics
also separate authentication from session/selector phases; adapter diagnostics
can send a test prompt after authentication succeeds.

| Evidence | Conclusion and next check |
| --- | --- |
| Chrome explicitly unauthenticated | Browser login needs attention; preserve saved gateway state. |
| Chrome verified, import missing/rejected | Extension capture, permissions, relay, or persistence path; inspect the typed import result. |
| Chrome verified, import saved but activation pending | Credential snapshot reached storage; browser activation is incomplete. Not proof of invalid credentials. |
| Chrome verified, Obscura authentication rejected | Inspect replay fidelity, expiry, partitioned cookies, local storage, origin/device binding and provider challenge. This alone does not establish an engine defect. |
| Obscura authentication verified, composer missing or draft not retained | DOM/selector/input integration failed after authentication. |
| Obscura authentication verified, Send succeeds but transport/response fails | Investigate WebSocket/fetch/stream support, application errors, and extraction selectors. Authentication may still need rechecking if the provider explicitly rejects the chat. |
| Either runtime has 403, timeout, challenge, or unknown schema | Inconclusive; distinguish a blocked request from an explicit account rejection. |
| Same exported snapshot works in a clean external Chrome control but fails in Obscura | Strong evidence for credential-replay/runtime compatibility or binding; isolate transport and page APIs next. |

A positive Chrome check is not proof of the exported cookie snapshot's
completeness. The probe always reports exported_snapshot_verified=false for
this reason. Compare eligible cookie attributes, expiry and import application
privately; do not publish cookie values, stable cookie digests, account IDs or
credential-bearing URLs. An independent control must actually replay the same
exported snapshot before it can establish that distinction.

## Current investigation

Only ChatGPT and Meta AI are active targets. Reuse existing saved sessions first.
Do not request resync merely because a composer or transport failed.
Current installed JSON failures (502) are not credential-expiry evidence.
Meta AI account proof and actual imported-session acceptance remain open.
