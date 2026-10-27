# Chat Gateway Browser Relay (Chrome MV3)

This is an early, user-initiated cookie relay. It does not capture passwords, MFA, page text, browser history, or unrelated-origin cookies. It does not run content scripts or collect localStorage. Provider session cookies are bearer credentials; treat the extension and gateway as trusted components.

## Setup

1. Run the gateway on loopback only and configure encrypted persistence:
   - `CHAT_GATEWAY_SESSION_STORE=vault`
   - `CHAT_GATEWAY_VAULT_KEY` as exactly 64 hex characters representing a persistent 32-byte key.
2. Load this directory in Chrome via **Extensions → Developer mode → Load unpacked**.
3. Copy the extension ID shown by Chrome and restart the gateway with `CHAT_GATEWAY_EXTENSION_ID=<32-character Chrome extension ID>`.
4. Start the gateway. Its one-time six-digit pairing code is printed once and expires after five minutes.
5. Open a logged-in provider tab, open the extension popup, enter the code, and pair.
6. Choose the matching provider and explicitly click **Import cookies from current tab**. Chrome will prompt for the active provider origin. Only cookies for that origin's approved provider domain are sent.

## Relay API

- `POST /relay/v1/pair` with `{"code":"123456"}` returns a revocable token once.
- `GET /relay/v1/status` reports only whether each provider has a saved session.
- `POST /relay/v1/sessions/<provider>` imports a cookie-only snapshot; encrypted vault mode is mandatory.
- `DELETE /relay/v1/sessions/<provider>` deletes the provider's vault and legacy record.
- `POST /relay/v1/unlink` revokes the extension token. Restart the gateway to create a new pairing code.

All endpoints require loopback Host validation and the exact configured `chrome-extension://<id>` Origin. Except for pairing, requests require the paired bearer token. The gateway never accepts an extension-supplied navigation URL.

## Limitations

- Cookie-only import is deliberate until provider-specific localStorage key allowlists are reviewed.
- Pairing is per gateway process; tokens live in `chrome.storage.session`, so restarting Chrome requires pairing again.
- This prototype requires a configured extension ID and the explicit environment-based vault key. OS keychain integration and production extension signing/review remain follow-up work.
