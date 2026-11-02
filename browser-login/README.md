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

## Supported native runtime; separate Docker boundary

The embedded browser is supported on Linux/Replit **natively**, not in the
default distroless Docker image. That image contains neither Node/Playwright
nor worker/browser assets and sets `CHAT_GATEWAY_EMBEDDED_BROWSER=disabled`.
Authenticated browser-ticket requests return HTTP 503 with
`embedded_browser_not_packaged`; the UI explains the native alternative.
Health/API and the independently secured extension relay remain available.
Overriding the flag cannot supply missing worker assets. The native-only-image
HTTP regression verifies this rejection still requires authorization.

From a clean feature-branch checkout, install Node >=22, Python >=3.10, an
OpenSSH client, Rust with rustfmt/Clippy, cmake, clang/libclang and a C/C++ compiler:

```sh
npm ci --ignore-scripts
npm audit --audit-level=high
# Replit already supplies sandbox-capable /repl/tools/bin/chromium.
# On a supported Ubuntu host instead:
# npx playwright install --with-deps chromium
bash scripts/native-rust.sh cargo fmt --all -- --check
bash scripts/native-rust.sh cargo build --locked
bash scripts/native-rust.sh cargo test --locked
bash scripts/native-rust.sh cargo clippy --locked --all-targets -- -D warnings
node --test browser-login/*.test.mjs
python3 -B -m unittest discover -s scripts -p test_native_login.py
python3 -B -m unittest discover -s browser-login -p 'test_*.py'
node browser-login/http-smoke.mjs target/debug/chat-gateway-rs
node browser-login/smoke.mjs target/debug/chat-gateway-rs --through-ssh
python3 -B scripts/run-native-login.py --diagnose
python3 -B scripts/run-native-login.py
```

`scripts/run-native-login.py` binds **127.0.0.1:8099 only**, rejecting remote
mode. Keep this checkout and its `browser-login/` files at their build location:
the binary references the worker in its compilation checkout. Do not copy the
binary alone to an unrelated host. `/dev/shm` must support private temporary
profiles and Chromium's sandbox must work; never add `--no-sandbox`.

The default persistent native state is `data/native-login/` (SQLite and encrypted
sessions), with 0700 directories/0600 files. `CHAT_GATEWAY_NATIVE_STATE_DIR`
selects an owner-approved private persistent path; `CHAT_GATEWAY_NATIVE_BINARY`
can select the corresponding build artifact. These are **non-secret** settings.
Do not assume an ephemeral container filesystem is durable; verify storage
availability/backups and retain the vault key separately under owner control.
Never point smoke tests at the persistent state. The launcher requires
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

The SSH variant starts a test-only SSH server on loopback with fresh ephemeral
host/client keys, pinned host-key verification and signed public-key
authentication. It runs the **production owner helper CLI** and remote Python
stdio command; the full real-gateway/browser suite crosses that encrypted pipe.
No existing SSH configuration, configured Secrets, provider state or host keys
are read. All generated keys are temporary fixtures, never printed or committed.
This verifies the transport, not the owner's actual desktop/Replit SSH trust,
account access or provider authentication. The owner must still establish that
connection using the Replit SSH tool. `--ssh-config` can select their local
OpenSSH config. Default local/remote gateway ports remain 8099; optional
`--local-port`/`--gateway-port` support other loopback ports without header
rewriting.

The Rust-only Nix workaround is `GLIBC_TUNABLES=glibc.rtld.optional_static_tls=1048576`
plus the installed GCC runtime library directory in `LD_LIBRARY_PATH`; the
native launcher/fixture platform helper apply it only when that Nix path exists.
Do not export it globally into Node/Chromium. On non-Nix hosts the fixtures use
the Playwright-installed executable unless the public
`CHAT_GATEWAY_CHROMIUM_PATH` explicitly selects another sandbox-capable Chromium.
`scripts/native-rust.sh` reproduces these build settings with a temporary HOME,
cleared environment and the configured public `CARGO_HOME`/`CARGO_TARGET_DIR`
caches. Put the installed Rust toolchain on PATH first; compiler/cache paths are
ordinary settings, never gateway Secrets. This script is part of the repository
and does not depend on a separate Replit runner checkout.

CI runs the portable native suite separately from Docker, on Ubuntu 22.04 with
Chromium's namespace sandbox enabled. No production provider or synthetic
fixture enables routing. Real-account acceptance, MFA/CAPTCHA and cookie-only
session compatibility remain owner tests, not CI success claims.
