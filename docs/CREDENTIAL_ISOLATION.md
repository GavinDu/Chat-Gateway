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
| Chrome verified on the **same source-bound capture generation**, Obscura authentication rejected after that snapshot was applied | Investigate replay fidelity, expiry, partitioned cookies, origin storage, device binding and provider challenge; this alone does not establish an engine defect. |
| Chrome verified but not bound to the exact applied snapshot generation | Inconclusive: do not blame the relay or Obscura without correlated source evidence. |
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

The offline `--diagnose-credential-evidence` classification accepts an optional opaque,
canonical `chrome.generation` field. A `replay_or_binding` verdict requires it
to equal the `snapshot.generation` and `obscura.generation`; otherwise an
account discrepancy remains `inconclusive` with a fixed reason. **Do not**
copy a generation identifier from unrelated gateway status into a manual Chrome
probe result: the read-only DevTools probe does not certify capture lineage.
Only source-bound extension capture/import evidence can establish that link.
The offline report always says `live_proof=false`; matching owner-supplied IDs
are not independently verified credentials.

## Versioned gateway failure attribution

New gateway owner status includes a fixed `source_failure_schema_version` in
`last_failed_request`, alongside the generated request UUID, stage, reason,
duration and observation time. Copy that field from the **actual installed
gateway's authorized status**, not from the local checkout or a guessed image.
The offline `credential-diagnose` evidence file's `gateway_failure` record must
carry the same integer. Older status without it, a zero marker for
unsupported producer labels, or a newer unknown version is
`source_schema_unverified`: it remains *inconclusive*, not proof of a broken
provider or expired session. This validation is enforced by the producer in
release builds as well as by the offline reader.

Version 1 source emission supports only candidate-admission busy/skipped
reasons, typed backend-admission reasons, typed-or-unclassified first-stream
reasons, and unclassified JSON-completion reasons. Any other stage/reason pair
is `inconsistent_gateway_stage_reason`. The offline tool also requires a
matching request UUID, chronological observations and matching snapshot
generation; its `live_proof` stays false. Do not put arbitrary upstream error
messages, cookies, bearer URLs, account identities or credential digests into
this report. Even a versioned status is not installed image/build attestation,
successful Chrome-to-Obscura replay or a verified signed-in Meta AI account.

## Current investigation

Only ChatGPT and Meta AI are active targets. Reuse existing saved sessions first.
Do not request resync merely because a composer or transport failed.
Current installed JSON failures (502) are not credential-expiry evidence.
Meta AI account proof and actual imported-session acceptance remain open.
