# Embedded provider login (local-only)

**Native loopback acceptance passed:** 23 real gateway/browser fixture groups,
including popup SSO, CAPTCHA iframe restrictions, concurrent provider isolation,
encrypted save, actual gateway-process restart/restore, revocation, crash
recovery and profile cleanup. Independent HTTP E2E passed 9 groups. Rust checks
passed with 366 unit + 3 integration tests. These are synthetic-provider tests,
not proof of real-account authentication or provider anti-automation compatibility.
The browser suite also checks painted fixture pixels on the canvas, including
after crash recovery; a received WebSocket frame or stale URL alone is not
accepted as proof that the user can see the browser.

This additive implementation uses Playwright-managed, sandboxed Chromium and
CDP screencast frames over an owner-authorized WebSocket. It does not use VNC,
an iframe of a provider website, or a separate user-facing Chrome window.
The existing browser-extension relay remains available as a secondary option.

## Runtime

Node 18+ (tested Node 24.22.1), Playwright 1.55.0, a compatible Chromium binary,
and writable Linux `/dev/shm` are required. The Replit workspace includes the
Node dependency in its root package lock and uses `/repl/tools/bin/chromium`.
The implementation checkout also includes portable package.json/package-lock
files: run `npm ci` there when moving it outside this workspace.
For a portable install, install Playwright 1.55.0 and its Chromium distribution,
or set `CHAT_GATEWAY_CHROMIUM_PATH` to a verified compatible browser.
The worker lives alongside this checkout; do not ship just the Rust binary.

Each Start creates its own Chromium process and private tmpfs profile/context.
The browser sandbox remains enabled. No fallback disables it. Node receives no
gateway credentials or vault encryption key. Input and frames are never logged.
The profile is deleted when the session closes; only encrypted vault snapshots
are durable. Existing provider profiles are not opened.

## Owner and transport

1. Configure the gateway's existing hashed API keys with an `admin` scope, or
   explicitly opt into `CHAT_GATEWAY_OWNER_AUTH_FROM_ENV=1` with
   `CHAT_GATEWAY_API_KEY`. The gateway hashes it internally, without writing the
   key or digest to a launcher/config/log. Existing configured auth keys and this
   mode cannot be combined. The environment key must have at least 32 printable,
   non-whitespace ASCII characters.
   Anonymous local mode is deliberately insufficient for this feature.
2. Require `CHAT_GATEWAY_SESSION_STORE=vault` and a valid vault key.
3. Open `/login` through a trusted loopback transport.
4. Authorize with the gateway admin credential. This is not a provider
   credential and stays in page memory (not storage or URLs).
5. Choose a provider and Start; type passwords/MFA only into the streamed page.
6. Save Session explicitly writes through the existing encrypted vault importer,
   then closes the ephemeral browser. Runtime activation is reported separately.
   Cancel/disconnect/idle expiry deletes the browser without saving.

The public HTML/JS shell contains no private data. Status, ticket issuance, and
legacy APIs retain API-key authorization. Ticket issuance checks provider scope.
The WebSocket requires an explicit same-origin Origin and an expiring, single-use,
provider-bound capability issued after admin authentication; no reusable API key
is put in a WebSocket URL. Existing Host/CSRF/remote-mode checks are unchanged.

**Remote mode remains blocked for `/login` controls.** Replit Preview is not
automatically a supported owner transport. Do not map this listener publicly,
rewrite trusted headers, or relax the loopback guard to make Preview work.

## Navigation and session limitations

Document requests (including frames and OAuth hops) use provider and reviewed
identity-provider domain allowlists, with a CDP Fetch guard for redirect hops.
Multipage code keeps SSO targets in the streamed app viewport, with a browser-
level CDP guard installed before targets resume. Chromium requires flat browser
auto-attach; the public Playwright transport uses explicitly attached legacy
target RPC sessions for guarded control. The popup test passes with tab
switching, input and opener messaging. The former timeout came from the test
runner clicking below the visible owner viewport after resize; pointer tests
now scroll the canvas into view and assert visible coordinates. No guard was
removed and no raw CDP endpoint is exposed.

Reviewed CAPTCHA document exceptions are iframe-only; address-bar navigation to
those domains is denied. Both policy and real-browser iframe/redirect tests pass.
Downloads and service workers are disabled. Paste is forwarded only from an
explicit canvas paste event, without clipboard permissions or storage.
WebAuthn/device authentication, IME and real provider challenges remain unverified.

Save/restore intentionally follows the existing relay vault contract:
provider-scoped cookies only, **no arbitrary localStorage import**. This is not
a claim that every provider can authenticate or recover using cookies alone.
Presence of cookies is not proof of successful provider authentication.
Cookie SameSite and expiry metadata round-trip without forcing Lax; legacy
snapshots without those fields remain readable.

The authenticated DELETE `/login/api/:provider/browser/session` invalidates
outstanding tickets, cancels an active login, waits for its save/cleanup to
finish, then revokes the saved session. Disabled-provider revocation never
opens a provider page. Warmed native providers retain the existing clear-before-
delete contract. HTTP authorization/idempotency/stale-ticket checks pass;
live-browser revocation and fresh-start behavior pass the
browser fixture suite. Admission/restore/revocation require vault mode plus a
valid key. Restore calls VaultSessionStore directly, never the legacy file-store
selector. Missing Origin and literal null Origin are rejected.

## Private owner access without a public listener

Run `owner_tunnel.py` on the owner's computer, **not in this workspace**, after
setting up and trusting the connection shown by Replit's SSH tool:

```sh
python3 owner_tunnel.py --ssh-target OWNER_SSH_TARGET --ssh-port SSH_PORT
```

Use the target/port or configured SSH host alias from that tool. The helper uses
the owner's existing SSH agent/config, requires host-key trust and batch-mode
authentication, and never requests, reads or prints key contents. If SSH needs
an identity option, configure it locally in the owner's normal SSH config.
Then open `http://127.0.0.1:8099/login` locally. Existing gateway authorization
remains mandatory. Stop the helper with Ctrl-C.

This is a byte-preserving SSH exec/stdio pipe to the gateway's loopback socket,
not an HTTP header-rewriting proxy and not SSH TCP forwarding. It does not rely
on an unverified AllowTcpForwarding setting or trusted cloud forwarded headers.
It binds only 127.0.0.1, limits connections, and does not start a public service.
Automated tests cover target validation, host/auth flags, HTTP/WebSocket byte
preservation, EOF and failure cleanup. The owner's actual desktop-to-Replit SSH
connection still needs owner verification; it was not attempted by the agent.

## Replit isolated implementation

Base revision: `c03f0ad8681b9394709ef014acb6d4d582e70cb3`.
The old pinned checkout, package, and runtime are unchanged. Modified application
source is in `generated-artifacts/embedded-login/source`, a separate local Git
checkout, not committed or pushed.

From the Replit workspace root:

```sh
bash e2e-runner/embedded-login.sh fmt
bash e2e-runner/embedded-login.sh build
bash e2e-runner/embedded-login.sh test
bash e2e-runner/embedded-login.sh clippy
node --test generated-artifacts/embedded-login/source/browser-login/*.test.mjs
bash e2e-runner/embedded-login.sh http
bash e2e-runner/embedded-login.sh bench-memory
bash e2e-runner/embedded-login.sh bench-provider
bash e2e-runner/embedded-login.sh smoke
```

`e2e-runner/run_embedded_login.py` is an opt-in loopback launcher on port 8099,
using a separate private-state directory. It requires the existing
`CHAT_GATEWAY_VAULT_KEY` and `CHAT_GATEWAY_API_KEY` Secrets. The launcher tests
their names for presence only; Rust consumes/validates them normally in process.
An optional **non-secret** `CHAT_GATEWAY_AUTH_SECRET_HASH` setting can select
explicit hashed config instead. Do not put a plaintext key in that hash setting.
`--diagnose` reports only presence, public settings and missing/invalid names.
No existing provider sessions are linked by this launcher.

Provider routing remains disabled unless the owner explicitly sets
`CHAT_GATEWAY_ENABLED_PROVIDERS` to a comma-separated subset of the known eight
provider IDs. Disabled routing does not prohibit manual login preparation.
Health 200 is not readiness or proof of provider authentication.

The smoke runner launches the actual Rust binary, real sandboxed Playwright
Chromium and owner UI, and uses a test-only Node preload shim to intercept all
provider requests with local synthetic fixtures. Production has no fixture
endpoint or allowlist bypass. It uses fresh test-only credentials/state and
does not inherit configured Secrets or contact a provider.

Build output was relocated to a task-owned `/tmp` cache after a workspace quota
failure, without deleting previous artifacts. That cache can expire; build
commands recreate its directory. The separate `package/chat-gateway-rs` binary
used by the owner launcher is retained in the workspace.
