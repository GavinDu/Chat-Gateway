# Chrome browser-relay extension MVP

This is the Chrome Manifest V3 client for ADR 0001. It transfers only cookies for the provider selected by the user. It does **not** capture passwords, MFA values, form contents, browser history, or page contents. localStorage transfer is deliberately disabled until provider-specific key allowlists are reviewed.

## Prerequisites

1. By default, run Chat Gateway on loopback only. Remote mode is an explicit, separately configured deployment described below.
2. Configure encrypted session storage before linking any provider:
   - Set `CHAT_GATEWAY_SESSION_STORE=vault`.
   - Provide the stable 32-byte vault key either as `CHAT_GATEWAY_VAULT_KEY` (exactly 64 hexadecimal characters) or as `CHAT_GATEWAY_VAULT_KEY_FILE` (a regular, non-symlink file with owner-only permissions).
   - Keep the key outside the repository, config files, logs, container image, and extension storage. Back it up securely; losing it makes the vault unreadable.
3. Load the unpacked `extension/` directory from `chrome://extensions` with Developer mode enabled.
4. Start the gateway. Enter its local relay URL in the extension, choose **Connect to gateway**, and approve the local confirmation tab once. The gateway remembers that approved extension ID; no pairing code or manual extension-ID setup is needed.
5. Choose **Connect account** for a provider to grant its permissions and reuse an existing session, or sign in on its official site if needed. Status and cookie updates refresh automatically.

An explicitly configured `CHAT_GATEWAY_EXTENSION_ID` still pins the installation to that ID. Legacy CLI pairing remains available for compatibility; a code is not needed for the local extension approval flow.

The default popup gateway URL is `http://127.0.0.1:8080`; adjust it if the configured gateway port differs. The extension accepts loopback HTTP or an explicitly configured HTTPS remote origin; it never permits a remote HTTP URL.

## Optional private HTTPS gateway (remote mode)

Remote mode is opt-in and is intended only for a private deployment behind a trusted TLS-terminating edge. It is not enabled by setting a public bind address alone.

Set these Replit/deployment environment values:

- `CHAT_GATEWAY_REMOTE_ORIGIN`: exact public origin, for example `https://your-private-app.example` (origin only; no path).
- `CHAT_GATEWAY_TRUSTED_PROXY=1`: explicitly opt into the trusted edge configuration.
- `CHAT_GATEWAY_TRUSTED_PROXY_IPS`: comma-separated exact IP addresses of the immediate TCP peers, for example `127.0.0.1,::1` for a same-host proxy. Hostnames, CIDR ranges, wildcard and multicast addresses are rejected. Use the address the gateway actually observes; forwarded client addresses never establish proxy identity.
- `CHAT_GATEWAY_PROXY_KEY`: an independently generated high-entropy edge secret with at least 32 printable non-whitespace ASCII characters, distinct from the gateway API key. Store it in the edge/backend deployment secret managers. The edge must strip any incoming `X-Chat-Gateway-Proxy-Key` and forwarded headers, then inject its own `X-Chat-Gateway-Proxy-Key` and `X-Forwarded-Proto: https` after TLS termination. Never expose this key to browsers or the extension.
- `CHAT_GATEWAY_API_KEY`: high-entropy gateway API credential, at least 32 non-whitespace characters. Keep it in the deployment secret store; never commit or paste it into source.
- `CHAT_GATEWAY_EXTENSION_ID`: exact 32-character Chrome extension ID.

The gateway fails startup if any remote-mode setting is missing or malformed. It binds to the container interface only in this explicit mode, checks the exact configured Host, requires an allowlisted TCP peer plus the independent edge credential and trusted proxy HTTPS signal, and rejects other browser Origins. Remote `/login` and `/login/api/*` interactive controls remain disabled. Non-relay API requests require a Bearer credential matching `CHAT_GATEWAY_API_KEY`; the extension relay uses its own one-time pairing code and revocable install token. Pairing is accepted only from the configured extension origin. The extension asks Chrome for optional access to the exact HTTPS gateway origin during the user-initiated pairing action.

Do not expose the gateway process directly to the internet or trust arbitrary forwarded headers. If the hosting platform cannot guarantee proxy header sanitization and private backend routing, keep remote mode disabled. Local loopback behavior remains the default. A previous `CHAT_GATEWAY_TRUSTED_PROXY=1` setting alone is insufficient: existing remote deployments must add both identity settings before restarting. Requests missing TCP connection identity, carrying an incorrect edge secret, or repeating edge/Host/Origin/TLS headers fail closed. The gateway consumes the edge credential before dispatch and never includes it in errors. Rotate/revoke the edge secret by updating both secret stores and restarting the gateway; the old secret immediately stops authenticating after restart. Restrict backend reachability to the edge as defense in depth. If your host cannot inject an independent secret and provide stable immediate-peer addresses, keep remote mode disabled and use the local flow.

## Link and revoke

1. Sign into the provider normally in Chrome.
2. Open the extension popup, enter the gateway URL and one-time pairing code, then pair.
3. Select a provider and choose **Review permissions & link**. Chrome asks for that provider's optional host permissions; granting them is an explicit consent step.
4. The extension imports the provider-scoped cookie set into the encrypted vault and debounces later cookie-rotation updates. The gateway refreshes the embedded Obscura page after a successful import, so the new session takes effect without a process restart. Cookie updates are debounced and imported through the same authenticated relay.
5. **Unlink selected provider** clears provider-scoped cookies and origin localStorage from the embedded browser, replaces the warm page, removes active login pages, and then deletes encrypted and legacy on-disk records. If in-memory cleanup fails, the gateway returns an error and retains the saved records so it does not report false revocation.
6. **Unpair this extension** revokes the installation token. It does not delete provider records; unlink providers first if you want their vault entries removed.

In loopback mode, relay requests are restricted to loopback. In remote mode, they additionally require the exact configured gateway Host and the allowlisted immediate TCP peer, independent edge credential and trusted proxy's HTTPS signal. Both modes check the exact `chrome-extension://<id>` Origin. Paired requests require a bearer install token, timestamp, and one-use nonce. The token is returned once and stored in Chrome extension-local storage; the gateway persists only its SHA-256 hash in the owner-only `sessions/relay-install-token.sha256` file so pairing survives restart. Unpair deletes the server-side hash and revokes the token. Protect the browser profile.

## Current limitations / rollout gate

- Chrome only; Firefox and Safari are out of scope.
- Cookies only. localStorage is rejected until each provider has a reviewed key allowlist.
- The vault uses `ring` AES-256-GCM. Key inventory, deletion, rotation, and backup restore commands are available; production key retrieval is delegated to the deployment secret manager or a protected key file rather than a built-in OS-keychain integration.
- Provider unlink includes embedded-browser in-memory cookie/storage cleanup. Real-account restart restoration and live unlink/revocation still require validation against linked provider sessions.
- Provider automation must be permitted by the provider's terms and account policy. The extension must not be used to evade access restrictions. Keep unsupported providers disabled.
- No live provider test is implied by CI. Use recorded fixtures and CI for routine verification; perform any live-provider check only with an account you control and where the provider permits it.

## Extension connection and provider status

Enter the local relay URL in the extension and choose **Connect to gateway**. A local approval tab asks you to approve that configured extension once. No pairing code or separate provider login dashboard is needed. The request expires after five minutes; its approval and token exchange are single-use. The extension keeps its connection in the browser profile and reconnects after browser/gateway restarts. Legacy command-line pairing remains available for compatibility.

Provider status is shown in the extension and refreshed while its popup is open. Choose **Connect account** to grant that provider's browser permissions. An existing browser session is synced immediately; otherwise the official provider site opens for sign-in. Cookie changes and periodic refresh keep connected sessions synced afterward. Passwords, MFA and CAPTCHA remain on the official provider site. A saved session or browser cookies are reported separately from a gateway-verified login; cookies alone do not prove authentication.
