# Chrome browser relay extension (MVP scaffold)

This Manifest V3 extension is Chrome-first and uses explicit provider consent. It requests only approved provider origins and the local loopback gateway. It never reads passwords, form values, browser history, or unrelated-origin cookies. Provider local-storage key allowlists remain empty until justified and reviewed.

## Development

1. Start Chat Gateway locally on `127.0.0.1:8080`.
2. Open `chrome://extensions`, enable Developer mode, and choose **Load unpacked** → this directory.
3. Sign in normally at an approved provider site.
4. Enter the one-time pairing code printed by the local gateway, then select the provider and link the active tab.
5. Unlink from the popup when finished.

## Required gateway API (not implemented yet)

- `POST /relay/v1/pair` — exchange a short-lived pairing code for a revocable install token.
- `GET /relay/v1/status` — report linked-provider state without returning session material.
- `POST /relay/v1/session` — import a provider session snapshot.
- `DELETE /relay/v1/session/:provider` — revoke/unlink and delete the provider vault record.

**This extension is a scaffold and is not functional until the paired API implementation and its tests land.** The gateway must validate provider/origin correspondence and cookie domains, encrypt imports before acknowledging, reject unpaired calls, and redact secrets from errors and logs. Only loopback is supported; do not add a remote gateway URL or wildcard host permissions.
