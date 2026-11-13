## Tokenless local-only relay (Docker helper default)

The official `scripts/docker-run.sh start` helper now sets
`CHAT_GATEWAY_LOCAL_RELAY_NO_PAIRING=1`, while publishing the gateway **only
on host 127.0.0.1**. The extension preauthorizes local host origins and
automatically detects `GET /relay/v1/mode` on startup. No pairing code,
approval tab, install token, API key, or password is required for **extension ↔
local gateway** communication. Previously paired local extensions are migrated
automatically; they must be reloaded after updating the extension and gateway.

The local API exposes:

- `GET /relay/v1/mode` → `{"mode":"local","no_pairing":true}`.
- `GET /relay/v1/status` and `GET /relay/v1/diagnostics` → redacted live
  session status, from localhost tools such as `curl`, with no token or
  special headers.
- Extension session POST/DELETE endpoints require a real
  `chrome-extension://<32-character-id>` browser Origin, a fresh
  timestamp and unique request nonce, and remain subject to rate limits.
  Normal websites, remote browser origins and DNS-rebound Host headers are
  refused. These are request-integrity checks, **not a pairing flow**.

If other untrusted Chrome extensions are installed, optionally set
`CHAT_GATEWAY_EXTENSION_ID` before invoking `scripts/docker-run.sh start`.
This pins the local mutation API to exactly that extension. Without pinning,
any installed extension with localhost host permission can potentially invoke
the local relay API. Localhost is not isolation from other processes on the
same machine or containers sharing the gateway's Docker network; only use
tokenless mode in a trusted personal workstation environment. It is forcibly
disabled when `CHAT_GATEWAY_REMOTE_ORIGIN` is configured, retaining the paired
remote pathway. For a standalone native server, opt in explicitly by setting
`CHAT_GATEWAY_LOCAL_RELAY_NO_PAIRING=1`; its default bind stays loopback-only.

This removes **gateway/extension pairing**, not provider website
authentication, Chrome's explicit provider-cookie permission prompts, or
encrypted-at-rest vault requirements. `/v1/*` OpenAI-compatible API-key
authentication is independent of the relay and is unchanged.


# Chat Gateway Chrome Relay: provider management

The relay is intended for accounts you are authorized to access. Provider
passwords and MFA codes remain on each provider's official website; browser
cookies are sensitive bearer credentials.

## Connect the extension

1. Start the local Docker gateway and reload the unpacked extension in Chrome.
2. Open the extension. It detects `http://127.0.0.1:8080` and displays provider status automatically.
3. If your gateway uses another **loopback** port, enter that URL and click **Connect to gateway**. No approval tab, pairing code, or install token is used.

`CHAT_GATEWAY_EXTENSION_ID` can optionally pin one Chrome extension's access.
Private remote HTTPS deployments still use the legacy paired connection flow.

## Connect or reauthenticate a provider

1. Once the gateway is connected, the extension displays a card for each provider with its gateway and authentication status.
2. For an unlinked provider choose **Log in / Connect** and grant its browser host permissions. If browser cookies exist, the gateway imports them immediately. Otherwise the extension opens the official provider website (or provides an **Open provider website** link), where you can sign in and complete MFA.
3. Future cookie changes sync automatically for that consented provider. Use **Sync session** for a manual refresh, **Open website** to sign in again, or **Re-login / Sync** when runtime status indicates that login is needed.
4. After linking, a small **Log out** button revokes that provider's saved gateway session and disables automatic cookie resync. The next **Log in / Connect** explicitly re-enables it. Logging out from Chat Gateway does **not** sign out your Chrome profile on the provider's website.

Use **Disconnect gateway** to stop this extension's local sync without deleting provider vault records. In legacy paired mode it also revokes the install token. Disconnecting and provider logout remain separate actions.

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

The local-only gateway exposes `GET /relay/v1/diagnostics` and
`GET /relay/v1/status` without a token, for read-only loopback clients.
The response includes per-provider `issue` codes and a top-level `issues`
array. Privileged extension session mutations still require a Chrome-extension
Origin, valid timestamp and unique nonce. Paired remote mode retains its
original install token checks. `linked` means a saved encrypted
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

To reduce periodic work, a connected popup refreshes its status every 10
seconds (and on focus or manual refresh). Pending pairing retains a
five-second check; the service worker's linked-provider repair interval is
15 minutes and cookie changes are debounced for 10 seconds per provider.
Successful cookie snapshots are fingerprinted locally using SHA-256 bound to the gateway and connection identity; only the digest is stored, never the session contents. Automatic refreshes skip identical snapshots even after service-worker suspension. Explicit **Sync session** bypasses this optimization for immediate operator recovery.
Browser activation timeout, a provider's 403 JavaScript asset failures, and
incompatible site JavaScript require separate diagnosis; reducing log volume
does not imply those errors are fixed.

