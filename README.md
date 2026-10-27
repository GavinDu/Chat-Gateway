# chat-gateway-rs v2.0.0 - Obscura Native

Rust port of Chat-Gateway with **8 providers**: Qwen, Meta AI, Claude,
ChatGPT, Gemini, Grok, Mistral, Perplexity.

Unified memory layer + OpenAI-compatible API + automatic failover,
running on the **embedded Obscura engine** (the `obscura` crate linked
in-process - no chromiumoxide, no external `obscura serve` CDP process).

## Quick start

```bash
cp config.example.yaml config.yaml
# Read docs/PROVIDERS_POLICY.md, then explicitly enable only providers
# whose terms permit your intended use; all providers start disabled.
# After enabling at least one provider, configure its session if required.
cargo run -- --config config.yaml
```

The server listens on `server.port` (default 8080, `PORT` env overrides).


## Security boundary: local login relay

The gateway binds to `127.0.0.1` only. This is intentional: the login UI and
`/login/api/*` endpoints can control provider browser sessions and persist
session cookies/localStorage, so they are **local-machine control surfaces**, not
a remotely accessible login service. The server does not currently support a
public or LAN bind address, and changing the bind address is not a supported
configuration option.

The HTTP middleware also rejects requests whose `Host` is not an explicit
loopback authority, rejects unsafe `Origin` values, and rejects cross-site
Fetch Metadata for login API routes. These checks defend against DNS-rebinding
and cross-site browser requests; they are defense in depth, not authentication.
Local automation clients that omit `Origin` are supported, but should only be
used by trusted local processes.

**Do not expose this service through a reverse proxy, tunnel, port-forward, or
container port publication to an untrusted network.** Remote access is not
supported until a separate design adds explicit authentication, TLS, and
origin/CSRF protections for the remote trust boundary. Do not log or share
provider session files: they may contain reusable authentication material.

## API (OpenAI compatible)

- `GET /health` - status + configured providers
- `GET /v1/models` - configured aliases + discovered catalog (discovery
  runs in the background at startup; picker labels are read without
  sending a message, failures fall back to the configured aliases)
- `POST /v1/chat/completions` - streaming (SSE) or JSON

```bash
curl http://localhost:8080/v1/models
curl http://localhost:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"Hello"}]}'
# auto = priority order from config; a model alias routes to its provider,
# and on failure the next eligible provider is tried.
```

Pass the `X-Conversation-Id` response header back on later turns to
continue the conversation: context is rebuilt from the unified memory
store (sliding window, ~8k tokens by default) and the conversation keeps
affinity to its last provider.

Responses include `X-Gateway-Provider` and `X-Gateway-Model` when the selected values are valid HTTP header values. Usage token counts are estimates (`x_usage_estimated: true`), not provider-billed token counts; streaming responses emit the assistant role in the initial chunk and usage in the final chunk.

## Architecture

```
Axum HTTP side (multi-thread)
  -> ObscuraPool (command channel + per-provider semaphores)
    -> browser thread (current-thread runtime + LocalSet)
      -> embedded Obscura Browser, one pre-warmed Page per provider
      -> per-provider mutex serializes sends on its page
      -> provider adapters (selector fallbacks) -> chunk channel back
  -> UnifiedMemory (SQLite-backed conversation store; sliding-window context)
```

Obscura pages wrap a V8 runtime behind interior mutability and are
neither `Clone` nor `Send`, so every page lives on the single browser
thread; the HTTP side only ever sees channels. Provider switch is a
map lookup + channel send on pre-warmed pages - no process or tab
spawn per request.

## Durable conversation storage (SQLite)

Conversation history now lives in `data/conversations.sqlite3`, not a rewritten
JSON snapshot. SQLite is the source of truth: reads use a consistent database
snapshot, and each turn (ordered messages plus provider/tab affinity) or deletion
commits as one transaction. Failed writes roll back completely. Independent
gateway connections serialize writes using `BEGIN IMMEDIATE` with a 30-second
busy timeout, reconciling transcripts against the latest committed history.
Identical completed-turn replays are idempotent; full and sliding-window
transcripts append only their unseen suffix.

`GET /v1/conversations/:conversation_id` exports the complete stored conversation
as JSON, including messages and provider/tab metadata. It returns `404` when the
conversation does not exist. Treat exports as sensitive data: they contain prompts
and responses and should be stored only in a protected location. `DELETE` on the
same path permanently removes the conversation from the active SQLite database;
older JSON or SQLite backups may still contain it.

`CHAT_GATEWAY_MEMORY_PATH` can select a database path. For compatibility, an
existing override ending in `.json` remains the **import source**, and its sibling
`.sqlite3` file becomes the database. Without an override, or with a direct
database override, the import source is `data/conversations.json`.

### JSON import and schema migrations

On first startup, the gateway creates schema version 1 (`PRAGMA user_version`)
and imports the legacy JSON snapshot in the **same transaction**. A completion
marker prevents reimport on later starts, including after conversation deletion.
The source JSON is never modified, renamed, or deleted automatically. Keep it
until you have verified history and affinity after restart, then archive it
securely or remove it yourself. Old JSON backups still contain deleted history.
After migration, restoring that JSON into a new empty database resurrects the
history in that backup; use a current SQLite backup for recovery.

Malformed JSON, mismatched conversation IDs, database corruption, foreign-key or
message-order violations, missing/modified schema definitions, and unsupported
schema/application versions stop startup with diagnostics instead of starting
with empty memory. A failed initial import leaves no committed schema or partial
history, and can be retried after correcting the source. Future schema upgrades
must be explicit, transactional migrations; unknown versions are never reset.

On Unix, database files are created/tightened to owner-only `0600`, and SQLite's
rollback journal inherits those permissions. Database symlink paths are rejected.
Use a private, trusted data directory on local storage; do not share it with
untrusted users or put it on a filesystem without SQLite locking guarantees.

### Backup and restore

Stop the gateway before copying the database, or use SQLite's online backup API:

```bash
sqlite3 data/conversations.sqlite3 ".backup 'conversations-backup.sqlite3'"
chmod 600 conversations-backup.sqlite3
```

Keep backups private. Do not copy only the main file while a transaction is
active, discard journal/WAL sidecars, or edit a live database. To restore, stop
all gateway processes, preserve the current database and all sidecars together,
restore the backup at the configured path with owner-only permissions, and
restart. Never run the older JSON-only gateway against this SQLite path.

Regression checks:

```bash
cargo test --locked memory::tests
cargo test --locked
cargo fmt --all -- --check
```

## Provider sessions

Sessions live in `sessions/<provider>.json`:
`{ local_storage, cookies: [{name, value, domain, path, secure,
http_only}], ts }`. At warm-up, cookies are restored into the browser's
cookie store **before** the first navigation, then localStorage is
injected after navigation to the provider origin, followed by a
reload-equivalent navigation. Note the cookie store is shared by the
one embedded browser (all providers), which matches how the pages are
warmed.

## Providers

All 8 adapters share one interaction core (`providers/common.rs`):
fallback selector lists for input/send/response/picker, picker-based
model discovery, evaluate-based typing (focus + `insertText`), and a
350ms response-poll loop that yields deltas and ends after ~2s without
growth and no streaming indicator. Qwen's input list is i18n-robust
(English + Chinese placeholder, test-id, contenteditable, bare
`textarea`). Perplexity answers are stripped of `[n]` citation
markers.

Test one adapter live:

```bash
cargo run -- --config config.yaml --test-adapter qwen
```

## Login UI (`/login`)

Open `http://localhost:<port>/login` in a browser to log providers in
through the gateway itself — the Rust-form successor to Go's dashboard
login flow. The page lists all 8 providers with session status
(`sessions/<provider>.json` present?) and login state, and lets you
Start / Cancel / Complete a login session per provider.

How a login works:

1. Press **Start** for a provider. The gateway opens a dedicated page
   for it on the browser thread (separate from the warm provider pages,
   so in-flight sends are not disturbed) and navigates to the
   provider's chat URL; a logged-out user is bounced to the provider's
   own login screen by the provider.
2. Follow the page from the **Active login session** panel: it shows the
   live URL, title, detected form fields and buttons, and a text
   snapshot, refreshed by polling. Drive the page with relay actions —
   click-by-selector, type-into-selector, Enter, click-a-listed-button,
   or navigate to another https URL for SSO hops.
3. When the panel reports **likely logged in** (best-effort signal: the
   URL is not a login URL *and* the provider's chat input is present),
   press **Complete**. The gateway captures cookies (from the
   browser-wide CookieStore) + localStorage into
   `sessions/<provider>.json` — the exact format the normal send path
   restores — and swaps the login page in as the provider's warm page,
   so the next send boots with the new session immediately. Idle login
   pages are dropped after 30 minutes without UI contact.

API (all JSON except the page itself):

- `GET /login` — the dashboard page (single self-contained HTML file).
- `GET /login/api/status` — all providers: enabled, session present
  (+ timestamp), login active, current URL, title, logged-in signal.
- `POST /login/api/:provider/start` — open a login page (replaces any
  previous one for that provider).
- `POST /login/api/:provider/cancel` — drop the login page.
- `POST /login/api/:provider/complete` — capture the session + swap the
  warm page.
- `GET /login/api/:provider/view` — live view data (URL, title, state,
  fields, buttons, snapshot; field *values* are never read).
- `POST /login/api/:provider/act` — one relay action
  (`{action: click|click_button|type|enter|navigate, selector?, text?,
  url?, index?}`).

Honest limits of the login UI:

- **Relay-only, no screenshots.** The public obscura API exposes no
  screenshot method (it exists only in the inner obscura-browser crate,
  unreachable because `obscura::Page` wraps its inner page in a
  `pub(crate)` field) and no keyboard/mouse API. A screenshot path
  would require driving pages through the inner crate directly — a
  second, separate browser stack that shares neither the pool's pages
  nor its cookie store — so it was deliberately not built. You steer by
  the URL/title/fields/snapshot view, not by pixels.
- **No credential handling by the gateway.** There is no
  username/password form here. Relayed text lives in memory only for
  the action that types it; it is never logged, stored, or echoed back.
  Prefer SSO / passkey options where the provider offers them.
- **Captchas, 2FA prompts, and SSO popups cannot be solved by the
  relay.** Type a 2FA code into the code field when it appears; if a
  provider's bot checks refuse synthetic input (dispatched
  KeyboardEvents are not trusted keystrokes), that provider cannot be
  logged in through this UI and needs a session captured elsewhere.
- **Logged-in detection is best-effort** (URL keywords + chat-input
  presence), not a guarantee. **Cookies are browser-wide** in this
  engine, so a captured session file may contain other providers'
  cookies too; restore is domain-scoped, so this is harmless but worth
  knowing. Sessions expire — when a provider reports
  `session_expired`, re-capture it here.

## Performance benchmarks

Run the reproducible, provider-independent SQLite memory microbenchmark:

```bash
cargo bench --locked --bench memory_sqlite
```

It reports average, p50, p95, and throughput for durable `store_turn`,
context construction with a 500-turn transcript, provider/tab affinity reads,
and deletion. Each run uses a unique temporary directory and never opens the
repository's real data or provider-session files. Results depend on the host,
filesystem, Rust toolchain, and workload. This is a memory-layer microbenchmark,
not an end-to-end provider TTFT or browser startup benchmark.

A reference CI run on GitHub Actions `ubuntu-24.04` (Ubuntu 24.04.5, x86_64,
Rust 1.99.1, optimized bench profile; 500 samples per operation) reported:

| Operation | Average | p50 | p95 |
| --- | ---: | ---: | ---: |
| SQLite `store_turn` | 1.03 ms | 1.01 ms | 1.26 ms |
| `build_context` with 500 stored turns | 446.94 µs | 444.39 µs | 474.90 µs |
| Provider + tab affinity lookup | 648.29 µs | 647.63 µs | 661.49 µs |
| SQLite delete | 862.71 µs | 838.62 µs | 1.02 ms |

These are one CI run's observed values, not performance guarantees or regression
thresholds. Repeat on the target deployment hardware before making capacity or
latency decisions.

## Notes / honest limits

- One persistent page per provider: concurrent sends to the same
  provider are serialized on that page (bounded by
  `session_pool_size` permits on the HTTP side).
- Provider startup, resident memory, provider switching, and TTFT are not
  benchmarked by this harness; do not treat historical design targets as
  measured results.


## Local security boundary

The HTTP server binds to `127.0.0.1` only. The login relay can control provider pages and capture provider session data, so do not expose this listener through a public interface, reverse proxy, or port-forwarding tunnel. The login API rejects non-loopback `Host` authorities and cross-origin browser requests; these checks are defense in depth, not authentication for remote access. Remote access is not supported unless a future change adds explicit authentication and TLS. Local command-line clients may omit `Origin`; browser-origin requests must match the loopback request authority.


## Optional local API-key authentication

The server binds to loopback only. For shared local development environments, configure `auth.keys[]` with a key name, the lowercase SHA-256 hash of its secret, and one or both scopes (`chat`, `admin`). Store only the hash in YAML; generate a one-time secret with `chat-gateway-rs --create-api-key NAME` and save the printed secret immediately. The plaintext is not persisted or logged. 

When `auth.keys` is non-empty, every route except `/health` requires `Authorization: Bearer <secret>`. Chat routes require the `chat` scope; login/session-control routes and conversation export/deletion require `admin`. An empty key list disables API-key auth for backward-compatible loopback-only use; Host/Origin checks are not authentication and the listener must not be exposed remotely.

## Conversation memory, export, and retention

Treat exported conversations as sensitive user data; protect access to exports and include them in backup-retention and deletion procedures.

Conversation turns are persisted in the gateway's SQLite memory store so the API can rebuild context across requests. The gateway does **not** currently apply an automatic age-based retention period; stored conversations remain until explicitly deleted or the backing store is removed under the operator's data-management procedure. Operators should account for this when sizing disks, backing up the database, and defining their own retention requirements. Adding automatic TTL requires a separate purge implementation and backup-retention policy; this documentation does not imply that either is active.

- `GET /v1/conversations?limit=50&offset=0` lists conversation IDs, provider names, and message counts without returning message bodies. `limit` defaults to 50 and is capped at 100; `offset` defaults to 0.
- `GET /v1/conversations/:conversation_id` exports the stored conversation record, including its messages and provider/tab metadata, as JSON.
- `GET /v1/conversations/:conversation_id/export.md` exports the same conversation as Markdown with role-labelled messages.
- `GET /v1/conversations/:conversation_id/provider-threads/:provider` reads a provider-specific remote-thread mapping; `PUT` upserts it. Both require an `admin` API key when authentication is enabled. Writes accept only HTTPS URLs on the expected provider host.
- `DELETE /v1/conversations/:conversation_id` deletes a conversation from the live memory store. Deletion from separate backups is governed by the operator's backup-retention policy. Clients should treat an export as a data copy and apply their own access controls and deletion policy to downloaded files.
- When API-key authentication is enabled, conversation listing, export, and deletion require the `admin` scope. Treat exported JSON and SQLite backups as potentially sensitive conversation data.
- An unknown conversation returns `404 conversation_not_found`; malformed IDs return a client error. Export does not include provider cookies or session files.

There is no built-in TTL or automatic backup purge today. Do not interpret live-store deletion as proof that copies in backups have been erased.

## Metrics and request timing

The loopback-only `/metrics` endpoint exposes Prometheus text metrics for HTTP request counts and handler latency, plus provider queue wait, time to first token (TTFT), provider work, total chat duration, estimated gateway overhead, and the one-hot `gateway_provider_session_state` gauge (`unknown`, `ready`, or `needs_login`). When API-key authentication is enabled, `/metrics` requires an `admin` scope. Route labels are normalized to a bounded vocabulary to avoid high-cardinality metrics.

See [docs/METRICS.md](docs/METRICS.md) for metric definitions and interpretation.

## Provider pacing and challenge protection

Each provider has a configurable `min_request_interval_ms` (default `5000`) and `daily_request_cap` (default `100`) in `config.yaml`. The gateway reserves request slots before sending and applies bounded jitter between sends. A provider that reaches its UTC-day cap is skipped until the counter resets at UTC midnight. A detected CAPTCHA/security challenge disables that provider for the lifetime of the process and requires operator review/restart; the gateway does not attempt to solve or bypass the challenge. These defaults are conservative starting points, not a guarantee that a provider's own policies permit automation.

## Provider session health

`GET /health` reports each configured provider's runtime `session_state`: `unknown` until a successful send, `ready` after a successful response, or `needs_login` after an explicit `session_expired` provider error. `needs_login` providers are skipped by routing and do not count toward `/ready`. After completing login, restart the gateway to clear the in-memory session state.

Provider send selectors avoid generic submit buttons and icon-only guesses when possible. If a provider's specific send selector is unavailable, the adapter's bounded Enter-key fallback is used instead of clicking an unrelated control.
