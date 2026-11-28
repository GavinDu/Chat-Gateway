# Pinned native Obscura corrections

These three Apache-2.0 crates are copied from
`h4ckf0r0day/obscura` commit `0e2ee76383b95a14e51203799c4abc6790d6c7df`.
All other Obscura crates remain Git dependencies at that same revision.
Cargo source patches unify the local and upstream crate identities.

`UPSTREAM_SHA256.json` records the original bytes of every copied file.
`changes.patch` contains the runtime and crate-manifest differences against
that upstream revision. The parent Cargo.toml groups this isolated three-crate
workspace. Each carried crate declares explicit package/dependency metadata,
including versions and the pinned Git revision, so dependency auditors need
not infer inherited workspace entries. These local copies are not published
as crates. The original license is included.

Changes:

- Scripted native fetch/XHR supplies fetch metadata derived from the real
  initiator and target, instead of inheriting navigation defaults. Existing
  Origin, credentials, cookie, redirect, CORS, and SSRF checks remain active.
  Referrers contain only an HTTP(S) origin; HTTPS-to-HTTP referrers are omitted.
- Native event-stream responses yield bounded chunks before EOF. Ordinary
  non-event-stream responses retain the buffered interface. Resource
  cancellation closes a pending read; clones share one bounded native source.
  Response observers receive the bounded completed body, as before.
- The embedded stream shim drives underlyingSource.pull with backpressure.
  Native bridge helpers stay private to the bootstrap closure.
- The stable default JS/WebSocket browser profile matches the native HTTP
  transport's Chrome 145 Windows profile. Explicit profile overrides and
  opt-in rotation retain their upstream behavior.

`tests/native_fetch_pipeline.rs` uses only disposable loopback fixtures to
check metadata, ordinary JSON parsing, included/omitted cookies, CORS rejection,
private bridge isolation, first event before EOF, clone consumption,
cancellation, and the body limit. These focused checks do not substitute for
the upstream complete native suite or live provider acceptance.

No provider hostname exceptions, challenge solver, Chromium runtime, or
saved-account data are included in these crates.

## Preserved main cookie-scope correction

The cookie jar internally records host-only scope, but its public export
previously discarded that distinction. The gateway then saved Domain cookies
as host-only cookies, breaking saved-session recovery on provider subdomains.
Exported domain-scoped cookies now retain their leading dot; host-only cookies
remain unprefixed. The export reads the actual jar scope and never guesses from
an older snapshot, cookie name, or credential value.

`tests/native_cookie_snapshot.rs` checks two export/replay generations: Domain
cookies continue to reach a subdomain, while host-only cookies never do. No
provider accounts or network requests are used. Expiry and SameSite visibility
in the gateway wrapper remain separate limitations; this change makes no claim
of verified provider login.
