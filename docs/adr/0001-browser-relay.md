# ADR 0001: Browser-relay login protocol and threat model

- **Status:** Proposed — owner decision required
- **Date:** 2026-10-25
- **Scope:** Browser session linking and use of provider web sessions by Chat Gateway

## Context

The gateway currently drives an embedded Obscura page for login. That approach cannot reliably complete passkeys, CAPTCHAs, trusted user gestures, or some identity-provider flows. The intended model is for the user to authenticate in their normal browser and explicitly link the resulting provider session to the local gateway.

The gateway remains a local, loopback-only service. Provider web sessions are sensitive bearer credentials: possession can grant access to a user's provider account without the password or MFA challenge.

## Options considered

1. **Browser extension → encrypted session vault (recommended first milestone).** A Chrome extension reads only explicitly linked provider origins and transfers session material to a paired local gateway. The gateway stores it encrypted and restores it into the browser engine. This supports normal browser authentication and makes linking/revocation explicit.
2. **Live-tab relay.** The extension asks the user's real tab to execute sends and stream results. This reduces session copying but creates a more complex long-lived protocol and couples gateway availability to the browser tab.
3. **Chrome remote debugging (CDP).** Convenient for prototypes, but attaching to an existing profile is inconsistent across Chrome versions and exposes broad browser control to anyone who can reach the debugging endpoint. Not the default design.
4. **Manual cookie import.** Useful only as a documented recovery/debug path; easy to mishandle and difficult to rotate safely. Not the normal user flow.

## Proposed decision

- **Browser:** Chrome first. Firefox/Safari are out of scope for the first implementation until the extension and session semantics are stable.
- **Topology:** Run the gateway on the same machine as the browser, on loopback only. Prefer the same egress IP for browser authentication and gateway use where provider policy permits it. Do not expose the gateway through a public interface, reverse proxy, tunnel, or remote debugging port.
- **Transport/pairing:** Pair via a short-lived, single-use code initiated by the local gateway and confirmed by the extension. Bind the code to one gateway instance, expire it quickly, rate-limit attempts, and never log it. Require explicit user confirmation for each provider origin. Do not allow arbitrary extension-supplied URLs.
- **Session transfer:** Transfer only the minimum provider-specific material required by the adapter: cookies for the approved provider domain and, only where demonstrably necessary, selected local-storage keys. Never export passwords, form values, unrelated-origin cookies, browser history, or page contents. User-agent/language/timezone are metadata, not authentication secrets; collect them only when required and document them.
- **Storage:** Encrypt session material at rest with a key protected by the host OS when available. Restrict file permissions. Do not store plaintext session dumps or include session material in logs, crash reports, telemetry, or diagnostics. If secure key protection is unavailable, fail closed or require an explicit development-only mode rather than silently storing plaintext.
- **Revocation:** Unlinking deletes the provider's vault entry and invalidates pending pairing codes. Session expiry, provider challenge pages, extension disconnect, and vault-decryption failure must disable that provider and report a non-secret diagnostic. Rotated cookies should be persisted atomically.
- **Execution:** Keep provider actions scoped to the linked provider. The gateway must not accept arbitrary remote commands or navigate to arbitrary origins. Use the extension only for explicitly approved provider domains.
- **Fallback:** The current embedded login relay is a compatibility/debug path only. Text typing is disabled by default; see `CHAT_GATEWAY_LEGACY_LOGIN_RELAY`. Do not use it for passwords or MFA secrets.

## Threat model

| Threat | Main risk | Required mitigation |
|---|---|---|
| Malicious provider page | Attempts to redirect the extension, steal pairing data, or trick it into exporting unrelated cookies | Exact origin allowlist; validate sender tab and origin on every message; no wildcard host permission; never accept arbitrary URLs or page-directed export requests |
| Other local process | Calls local APIs or races pairing/revocation | Loopback bind; Host/Origin validation; scoped API keys where enabled; single-use, short-lived pairing codes; constant-time secret checks; rate limits |
| Compromised extension | Reads or exports linked sessions | Minimal permissions; no remote code; narrow message schema; explicit provider consent; reviewable builds; clear unlink controls; do not grant broad browsing/history permissions |
| Stolen vault/file backup | Reuse of provider sessions | Encryption at rest; OS-protected key; restrictive permissions; exclude vault/key from backups and support bundles by default; rotation/revocation instructions |
| Logs or diagnostics | Secret leakage | Redact authorization headers, cookies, storage values, prompts, response bodies, pairing codes, and secrets; test redaction |
| Malicious or compromised gateway | Uses linked sessions outside intended actions | Local trust boundary must be explicit; per-provider capabilities; no public listener; keep gateway and extension versions auditable; provide a reliable unlink/revoke workflow |

## Provider capability matrix (initial target)

| Capability | Chrome extension | Gateway |
|---|---|---|
| Complete sign-in, MFA, passkey, CAPTCHA | User in real browser | No password/MFA capture |
| Read cookies | Only approved provider origin after user links it | Receive minimum allowlisted cookie set |
| Read local storage | Only approved provider origin and allowlisted keys | Store only keys required by that adapter |
| Refresh rotated cookies | Observe approved-origin cookie changes and send debounced updates | Validate origin/provider, update vault atomically |
| Execute prompts | Not in first milestone; live-tab relay is a separate option | Obscura adapter using the linked session |
| Revoke | User unlinks provider in extension | Delete vault entry and clear in-memory session state |

The exact local-storage key allowlist, cookie requirements, and whether a provider supports reliable session restoration must be documented per adapter. Unsupported providers should remain disabled rather than silently broaden extension permissions.

## Consequences and open questions

- The extension + encrypted-vault path is the recommended first milestone; live-tab relay remains a later option if session transfer proves unreliable or violates provider policy.
- The gateway and browser should normally share a host and egress IP, but provider terms and account policies take precedence. The project must not attempt to evade provider access restrictions.
- A per-provider capability matrix is required before enabling each adapter.
- **Owner decision requested:** approve or amend the recommended extension + encrypted-vault first milestone, Chrome-first scope, and same-host topology before implementation is treated as final.
- Follow-up issues for the extension, vault, pairing, and live-tab relay should be reconciled only after this decision is approved.
