# Chat Gateway Chrome Relay: provider management

The relay is intended for accounts you are authorized to access. Provider
passwords and MFA codes remain on each provider's official website; browser
cookies are sensitive bearer credentials.

## Connect the extension

1. Start the local Docker gateway and load the extension in Chrome.
2. In the extension, enter the relay URL, normally `http://127.0.0.1:8080`.
3. Choose **Connect to gateway** and approve the local confirmation tab once.
4. Return to the extension. Its connection and approved identity are remembered across restarts.

No local pairing code or separate provider login dashboard is required. An explicit
`CHAT_GATEWAY_EXTENSION_ID` still pins the installation to that ID. Private remote
HTTPS deployments retain their existing code-based pairing protocol; local approval
is deliberately restricted to local gateways.

## Connect or reauthenticate a provider

1. Choose **Connect account** in that provider's status row and grant its browser permissions.
2. Existing browser cookies sync in the background, and the official provider website opens once to reuse its signed-in session or complete sign-in and MFA. Later refreshes do not open extra tabs.
3. Cookie changes sync automatically after sign-in; provider status refreshes in the popup. Use **Sync session** for a manual refresh or **Unlink** to remove the saved session.

The gateway must already have the relevant provider adapter **enabled in
`config.yaml`**, and your use must comply with that provider's applicable
terms. The extension cannot install arbitrary new provider adapters or enable
disabled ones. A provider not in the supported list requires an audited adapter
and a separately reviewed allowlist update.

## Reading status

- **Enabled in gateway** indicates configuration, not successful authentication.
- **Session saved** means an encrypted session snapshot exists.
- **Browser cookies present** does not prove an active login.
- **Login verified** / **Login needed** reflect the gateway's available embedded
  browser observation. In the normal Docker setup, a live observation may not be
  available; **Login unverified** is expected in that case.

The relay does not scrape passwords, bypass provider security controls, or
promise that a saved web session will keep working indefinitely.
