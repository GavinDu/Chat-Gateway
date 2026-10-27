# Obscura fingerprint replay: pinned-revision findings

**Status:** source-level capability review; no live provider experiment performed.  
**Reviewed revision:** `h4ckf0r0day/obscura@09790944a405ba81a7df27bc2577c6b0db283900`, the revision pinned by `Cargo.toml`.

## Findings

The public `obscura::BrowserConfig` and `BrowserBuilder` at the pinned revision expose:

- `stealth(bool)`
- `user_agent(String)`
- `proxy(String)`
- `storage_dir(PathBuf)`

They do **not** expose public language or timezone configuration fields/builders. The gateway's current `src/browser/pool.rs::build_browser` sets stealth and optional storage, but does not set a user-agent override. Therefore, today we cannot faithfully replay all three values (UA, language, timezone) through the public pinned API. A user-agent override appears supported; language/timezone replay is not established by this public API review.

This does not prove that the underlying engine has no internal or future support. It means the gateway should not claim to restore these values until a supported, tested public API exists.

## Operational constraint

A session copied from a normal browser may be bound to more than cookies: account risk controls can consider IP/egress, user-agent, locale, timezone, browser capabilities, and server-side risk signals. Matching visible values is not a guarantee of session portability. Run the gateway on the same trusted machine and network egress as the browser that established the session when possible; even then, expect re-authentication and do not attempt to evade a provider's access controls.

## Recommended next steps

1. Keep provider sessions isolated and revocable; never log cookie values or storage contents.
2. Add a documented, explicit UA override only if the original UA can be captured reliably and the provider's terms permit this workflow.
3. Do not synthesize language/timezone fingerprint values with page JavaScript as a substitute for engine-level support: request headers and browser APIs can disagree.
4. If language/timezone support is required, propose an upstream API and pin a reviewed revision before implementing it in this repository.
5. Validate only with accounts and providers whose terms permit automated testing; record results per provider and do not treat a single successful run as proof of general portability.

## Verification boundary

This is a source-level API review against the exact pinned revision, not an empirical anti-bot or session-portability test. The live experiment remains intentionally unclaimed.
