# Chat Gateway Chrome Relay: provider management

The relay is intended for accounts you are authorized to access. Provider
passwords and MFA codes remain on each provider's official website; browser
cookies are sensitive bearer credentials.

## Pair the extension

1. Start the local Docker gateway with `CHAT_GATEWAY_EXTENSION_ID` set to the
   32-character ID shown in `chrome://extensions`.
2. In the extension, enter the exact gateway origin, normally
   `http://127.0.0.1:8080`. The local Docker default is **8080**, not 3000.
3. Generate a fresh, single-use code using
   `./scripts/docker-run.sh pairing-code` (after the pairing-rotation
   improvement is merged and the Docker image rebuilt).
4. Enter the eight-digit code promptly and select **Pair extension**.

The one-time code expires after ten minutes and has a maximum of five failed
attempts. Never paste it or any session material into a public issue.

## Add or reauthenticate a supported provider

1. In the extension, select one of the eight configured providers.
2. Select **Add provider / Open login**, then complete login and any MFA on the
   provider's official site in Chrome.
3. Return to the extension and select **Review permissions & link**. Grant only
   the provider host permissions actually required.
4. Refresh the status dashboard. To update changed cookies choose **Sync
   session**; to remove saved credentials choose **Unlink**.

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
