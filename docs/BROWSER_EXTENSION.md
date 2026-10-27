# Chrome browser-relay extension MVP

This is the first Chrome Manifest V3 client for ADR 0001. It transfers only cookies for the provider selected by the user. It does **not** capture passwords, MFA values, form contents, browser history, or page contents. localStorage transfer is deliberately disabled until provider-specific key allowlists are reviewed.

## Prerequisites

1. Run Chat Gateway on loopback only.
2. Configure encrypted session storage before linking any provider:
   - `CHAT_GATEWAY_SESSION_STORE=vault`
   - `CHAT_GATEWAY_VAULT_KEY` set to exactly 64 hexadecimal characters representing a stable 32-byte key.
   - Keep the key outside the repository, config files, logs, container image, and extension storage. Back it up securely; losing it makes the vault unreadable.
3. Load the unpacked `extension/` directory from `chrome://extensions` with Developer mode enabled.
4. Copy the extension ID shown by Chrome and restart the gateway with `CHAT_GATEWAY_EXTENSION_ID=<32-character-extension-id>`. The ID must be the actual installed extension ID; the server rejects all other extension origins.
5. Restart the gateway and copy the one-time 8-digit pairing code printed at startup. It expires after ten minutes and permits at most five attempts.

The default popup gateway URL is `http://127.0.0.1:3000`; adjust it if the configured gateway port differs. The extension intentionally rejects non-loopback HTTP URLs.

## Link and revoke

1. Sign into the provider normally in Chrome.
2. Open the extension popup, enter the local gateway URL and one-time pairing code, then pair.
3. Select a provider and choose **Review permissions & link**. Chrome asks for that provider's optional host permissions; granting them is an explicit consent step.
4. The extension imports the provider-scoped cookie set into the encrypted vault and debounces later cookie-rotation updates.
5. **Unlink selected provider** deletes both encrypted and legacy on-disk records. Because the embedded browser may still have cookies in memory, restart the gateway after unlinking before relying on revocation. This limitation is intentional and remains a follow-up before the relay can be considered production-complete.
6. **Unpair this extension** revokes the installation token. It does not delete provider records; unlink providers first if you want their vault entries removed.

The gateway binds the relay routes to loopback and checks the exact `chrome-extension://<id>` origin. Paired requests require a bearer install token, timestamp, and one-use nonce. The token is returned once and stored in Chrome extension-local storage; protect the browser profile.

## Current limitations / rollout gate

- Chrome only; Firefox and Safari are out of scope.
- Cookies only. localStorage is rejected until each provider has a reviewed key allowlist.
- The vault currently uses the `ring` AES-256-GCM AEAD implementation and an explicit environment key. OS-keychain integration, key rotation/list commands, clearing the embedded browser's in-memory cookie jar on unlink, and live provider restore verification remain open work.
- Provider automation must be permitted by the provider's terms and account policy. The extension must not be used to evade access restrictions. Keep unsupported providers disabled.
- No live provider test is implied by this MVP. Use recorded fixtures and CI for routine verification; perform any live-provider check only with an account you control and where the provider permits it.
