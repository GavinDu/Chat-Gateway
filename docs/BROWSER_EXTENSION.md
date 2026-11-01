# Chrome browser-relay extension MVP

This is the Chrome Manifest V3 client for ADR 0001. It transfers only cookies for the provider selected by the user. It does **not** capture passwords, MFA values, form contents, browser history, or page contents. localStorage transfer is deliberately disabled until provider-specific key allowlists are reviewed.

## Prerequisites

1. By default, run Chat Gateway on loopback only. Remote mode is an explicit, separately configured deployment described below.
2. Configure encrypted session storage before linking any provider:
   - Set `CHAT_GATEWAY_SESSION_STORE=vault`.
   - Provide the stable 32-byte vault key either as `CHAT_GATEWAY_VAULT_KEY` (exactly 64 hexadecimal characters) or as `CHAT_GATEWAY_VAULT_KEY_FILE` (a regular, non-symlink file with owner-only permissions).
   - Keep the key outside the repository, config files, logs, container image, and extension storage. Back it up securely; losing it makes the vault unreadable.
3. Load the unpacked `extension/` directory from `chrome://extensions` with Developer mode enabled.
4. Copy the extension ID shown by Chrome and configure `CHAT_GATEWAY_EXTENSION_ID=<32-character-extension-id>`. The ID must be the actual installed extension ID; the server rejects all other extension origins.
5. Start the gateway and copy the one-time 8-digit pairing code printed at startup. It expires after ten minutes and permits at most five attempts.

The default popup gateway URL is `http://127.0.0.1:3000`; adjust it if the configured gateway port differs. The extension accepts loopback HTTP or an explicitly configured HTTPS remote origin; it never permits a remote HTTP URL.

## Optional private HTTPS gateway (remote mode)

Remote mode is opt-in and is intended only for a private deployment behind a trusted TLS-terminating edge. It is not enabled by setting a public bind address alone.

Set these Replit/deployment environment values:

- `CHAT_GATEWAY_REMOTE_ORIGIN`: exact public origin, for example `https://your-private-app.example` (origin only; no path).
- `CHAT_GATEWAY_TRUSTED_PROXY=1`: the edge proxy terminates TLS, strips client-supplied forwarded headers, sets `X-Forwarded-Proto: https`, and is the only network path to the gateway process.
- `CHAT_GATEWAY_API_KEY`: high-entropy gateway API credential, at least 32 non-whitespace characters. Keep it in the deployment secret store; never commit or paste it into source.
- `CHAT_GATEWAY_EXTENSION_ID`: exact 32-character Chrome extension ID.

The gateway fails startup if any remote-mode setting is missing or malformed. It binds to the container interface only in this explicit mode, checks the exact configured Host, requires the trusted proxy HTTPS signal, and rejects other browser Origins. Remote `/login` and `/login/api/*` interactive controls remain disabled. Non-relay API requests require a Bearer credential matching `CHAT_GATEWAY_API_KEY`; the extension relay uses its own one-time pairing code and revocable install token. Pairing is accepted only from the configured extension origin. The extension asks Chrome for optional access to the exact HTTPS gateway origin during the user-initiated pairing action.

Do not expose the gateway process directly to the internet or trust arbitrary forwarded headers. If the hosting platform cannot guarantee proxy header sanitization and private backend routing, keep remote mode disabled. Local loopback behavior remains the default.

## Link and revoke

1. Sign into the provider normally in Chrome.
2. Open the extension popup, enter the gateway URL and one-time pairing code, then pair.
3. Select a provider and choose **Review permissions & link**. Chrome asks for that provider's optional host permissions; granting them is an explicit consent step.
4. The extension imports the provider-scoped cookie set into the encrypted vault and debounces later cookie-rotation updates. The gateway refreshes the embedded Obscura page after a successful import, so the new session takes effect without a process restart. Cookie updates are debounced and imported through the same authenticated relay.
5. **Unlink selected provider** clears provider-scoped cookies and origin localStorage from the embedded browser, replaces the warm page, removes active login pages, and then deletes encrypted and legacy on-disk records. If in-memory cleanup fails, the gateway returns an error and retains the saved records so it does not report false revocation.
6. **Unpair this extension** revokes the installation token. It does not delete provider records; unlink providers first if you want their vault entries removed.

In loopback mode, relay requests are restricted to loopback. In remote mode, they additionally require the exact configured gateway Host and the trusted proxy's HTTPS signal. Both modes check the exact `chrome-extension://<id>` Origin. Paired requests require a bearer install token, timestamp, and one-use nonce. The token is returned once and stored in Chrome extension-local storage; the gateway persists only its SHA-256 hash in the owner-only `sessions/relay-install-token.sha256` file so pairing survives restart. Unpair deletes the server-side hash and revokes the token. Protect the browser profile.

## Current limitations / rollout gate

- Chrome only; Firefox and Safari are out of scope.
- Cookies only. localStorage is rejected until each provider has a reviewed key allowlist.
- The vault uses `ring` AES-256-GCM. Key inventory, deletion, rotation, and backup restore commands are available; production key retrieval is delegated to the deployment secret manager or a protected key file rather than a built-in OS-keychain integration.
- Provider unlink includes embedded-browser in-memory cookie/storage cleanup. Real-account restart restoration and live unlink/revocation still require validation against linked provider sessions.
- Provider automation must be permitted by the provider's terms and account policy. The extension must not be used to evade access restrictions. Keep unsupported providers disabled.
- No live provider test is implied by CI. Use recorded fixtures and CI for routine verification; perform any live-provider check only with an account you control and where the provider permits it.
