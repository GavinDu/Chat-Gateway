# Chat Gateway Chrome Relay: provider management

The relay is intended for accounts you are authorized to access. Provider
passwords and MFA codes remain on each provider's official website; browser
cookies are sensitive bearer credentials.

## Connect the extension

1. Start the local Docker gateway and load the extension in Chrome.
2. In the extension, enter the relay URL, normally `http://127.0.0.1:8080`.
3. Choose **Connect to gateway**. The extension opens a local approval tab; if the browser cannot open tabs from the popup, choose the **Open approval page** link instead.
4. Approve the request on the gateway page, then return to the extension. It switches automatically to the provider-status dashboard when the approval is redeemed.
5. If the popup closes while approval is pending, choose **Connect to gateway** again to reopen the same pending approval rather than creating a duplicate. Its connection and approved identity are remembered across restarts.

No local pairing code or separate provider login dashboard is required. An explicit
`CHAT_GATEWAY_EXTENSION_ID` still pins the installation to that ID. Private remote
HTTPS deployments retain their existing code-based pairing protocol; local approval
is deliberately restricted to local gateways.

## Connect or reauthenticate a provider

1. Once the gateway is connected, the extension displays a card for each provider with its gateway and authentication status.
2. For an unlinked provider choose **Log in / Connect** and grant its browser host permissions. If browser cookies exist, the gateway imports them immediately. Otherwise the extension opens the official provider website (or provides an **Open provider website** link), where you can sign in and complete MFA.
3. Future cookie changes sync automatically for that consented provider. Use **Sync session** for a manual refresh, **Open website** to sign in again, or **Re-login / Sync** when runtime status indicates that login is needed.
4. After linking, a small **Log out** button revokes that provider's saved gateway session and disables automatic cookie resync. The next **Log in / Connect** explicitly re-enables it. Logging out from Chat Gateway does **not** sign out your Chrome profile on the provider's website.

Use **Disconnect gateway** to revoke the extension's gateway authorization without deleting provider vault records. Disconnecting the gateway and logging out a provider are intentionally separate actions.

The gateway must already have the relevant provider adapter **enabled in
`config.yaml`**, and your use must comply with that provider's applicable
terms. The extension cannot install arbitrary new provider adapters or enable
disabled ones. A provider not in the supported list requires an audited adapter
and a separately reviewed allowlist update.

## Reading status

- **Enabled in gateway** indicates configuration, not successful authentication.
- **Session saved** means an encrypted session snapshot exists.
- **Browser cookies present** does not prove an active login.
- **Login needed** also uses the gateway's runtime session state, so an expired or missing session can be shown without opening the old login dashboard.
- **Login verified** requires an observed authenticated login. A saved session, cookies or a routable provider alone do not prove login; **Login unverified** is shown when there is no such observation.
- **Gateway check unavailable; retry** indicates a degraded runtime check, rather than claiming the account is signed out.

The relay does not scrape passwords, bypass provider security controls, or
promise that a saved web session will keep working indefinitely.


## Live diagnostics APIs

The authenticated gateway exposes `GET` and `POST /relay/v1/diagnostics`.
This is an alias of the existing protected `/relay/v1/status` response,
augmented with per-provider `issue` codes and a top-level `issues` array.
Use the paired extension install token and existing relay request protections
(Origin and nonce/timestamp requirements for POST); never expose this endpoint
without the normal relay authentication. `linked` means a saved encrypted
snapshot exists; `authentication: verified` requires observed login evidence.
`unknown` and `authentication_unverified` do **not** mean the account is logged
out. No cookies, browser storage, upstream URLs, prompt text, or raw errors are
returned.

Inside the extension, `chrome.runtime.sendMessage({type:"relay-diagnostics"})`
returns `{ok, extension, gateway}` on success, or `{ok:false, extension,
gateway_issue}` when not connected or the gateway cannot be checked. The
`extension.provider_sync` map contains only fixed issue categories, result
states and timestamps; it never contains session contents or install tokens.
This message is **internal to the extension**; there is no unauthenticated
external-messaging endpoint.

To reduce periodic work, a connected popup refreshes its status every 30
seconds (and on focus or manual refresh). Pending pairing retains a
five-second check; the service worker's linked-provider repair interval is
15 minutes and cookie changes are debounced for 10 seconds per provider.
Successful cookie snapshots are fingerprinted locally using SHA-256 bound to the gateway and paired install; only the digest is stored, never the session contents. Automatic refreshes skip identical snapshots even after service-worker suspension. Explicit **Sync session** bypasses this optimization for immediate operator recovery.
Browser activation timeout, a provider's 403 JavaScript asset failures, and
incompatible site JavaScript require separate diagnosis; reducing log volume
does not imply those errors are fixed.

