# Offline credential-versus-runtime diagnosis (redacted evidence only)

This is an **offline hypothesis classifier**, not a credential verifier. It does not
contact Chrome, Meta AI, ChatGPT, the Obscura worker, the relay, or the vault.
It never asks for API keys or resynchronizes credentials. Its output sets
`live_proof:false` even when all supplied claims indicate no observed failure.
Only source-verified first-party account checks can supply positive proof.

## Local invocation

Create a *redacted* `report.json` file containing fixed observations from one
active provider and a **random, non-credential-derived** snapshot generation
assigned by the operator/approved instrumentation. Never put cookies, tokens,
account IDs, raw error text, URLs, prompts, session hashes or HAR contents
in this file.

```json
{
  "provider": "chatgpt",
  "chrome": {"verdict": "verified", "observed_at_ms": 2000000000000},
  "snapshot": {
    "state": "applied",
    "observed_at_ms": 2000000000000,
    "generation": "d9f06fa4-62f1-4b9c-8718-96ac8e553d16"
  },
  "obscura": {
    "verdict": "verified",
    "observed_at_ms": 2000000000000,
    "generation": "d9f06fa4-62f1-4b9c-8718-96ac8e553d16"
  },
  "attempt": {
    "stage": "response",
    "outcome": "failed",
    "observed_at_ms": 2000000000000,
    "generation": "d9f06fa4-62f1-4b9c-8718-96ac8e553d16"
  }
}
```

The example timestamps are **fixtures**; replace them with fresh observation
times in UTC Unix **milliseconds** only after obtaining actual evidence.
Stale or future records, missing/mismatched generations, unsupported provider
IDs, unexpected fields and malformed JSON never yield a successful verdict.

```bash
chat-gateway-rs --diagnose-credential-evidence report.json
```

This operation runs before gateway config, network, provider pages, saved sessions
and vault keys are initialized. The input file is capped at 16 KiB and must not
be a symlink. Diagnostics are fixed-category only:

| Category | What the supplied evidence suggests |
| --- | --- |
| `browser_login_attention` | Explicit first-party Chrome account rejection (not a timeout) |
| `capture_or_import` | Snapshot missing/rejected, investigate extension and relay |
| `activation_pending` | Saved snapshot has not completed browser activation |
| `replay_or_binding` | Chrome verified while Obscura explicitly unauthenticated on the same applied generation; **not automatically an engine defect** |
| `adapter_input` | Obscura verified, but retained draft or Send failed |
| `transport_or_extraction` | Obscura verified, Send accepted, response/stream failed |
| `inconclusive` | Evidence missing, stale, contradictory, unsupported, or no failure observed |

The labels are **conditional on the truth of the input**. This tool cannot
validate that a caller's claimed `verified` observation came from the
actual authenticated account. Use the source-backed read-only Chrome control
for ChatGPT; Meta AI currently has **no established authoritative account
verifier** (#1329). Capture/import/replay parity (#1330) and installed
browser/Obscura correlation (#1331) remain separate requirements.

No local MCP sandbox workspace was accessed by this code slice. GitHub Actions
tests exercise only synthetic fixtures on exact PR commits; owner-installed
Mac runtime and signed-in browser E2E must be recorded separately. Keep other
providers deferred, and do not force resync because of a 403/502 or timeout.
