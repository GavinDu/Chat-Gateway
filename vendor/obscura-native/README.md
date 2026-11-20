# Pinned native cookie snapshot correction

This Apache-2.0 crate is copied from `h4ckf0r0day/obscura` commit
`0e2ee76383b95a14e51203799c4abc6790d6c7df`. All other Obscura crates
remain Git dependencies at that revision.

`UPSTREAM_SHA256.json` records the original source bytes. `changes.patch`
contains the exact correction and explicit crate metadata. CI verifies both
against the pinned upstream checkout. The original license is included.

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
