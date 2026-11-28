# ChatGPT provider and Meta AI performance handoff

## Current status

The capture, diagnostics and test-readiness changes are committed and merged through
`52e35250a621872dfdb2e4dfc9bd8334c8704324`. ChatGPT has passed fresh JSON,
fresh SSE and one distinct canonical replay in the disposable environment, but
later requests still time out without an assistant response. Full live acceptance
is unfinished. Issue [#1621](https://github.com/GavinDu/Chat-Gateway/issues/1621)
remains open.

The measurement workload contains **36 real OpenCode/Meta AI gateway attempts**:
30 protocol-complete streams and six incomplete attempts. Output correctness is
tracked separately. **No measured completion is under five seconds.**

This handoff commit preserves the remaining measurement tools, tests, numeric
records and operating context. It does not implement the proposed ChatGPT
`Response.body` correction. The next Meta AI coding call was prepared but was
**not executed**: automatic approval review could not complete because its usage
limit was reached. That is a review-service failure, not an unsafe-action verdict.
No additional provider request is counted for that blocked call.

## Branches, commits and pull requests

| Change | Feature head | Merged main | Result |
|---|---|---|---|
| Attempt phases/cancellation evidence, PR [#1622](https://github.com/GavinDu/Chat-Gateway/pull/1622) | `ab213d092b6d43d0fcf111af735418c5b150be2d` | `c9d9c252a4563ba9e58170b5f2e0e3af59266333` | Merged; disposable live replay passed, later memory request failed |
| Fixed network roles/dispatch observations, PR [#1623](https://github.com/GavinDu/Chat-Gateway/pull/1623) | `3ca15e996e7b1a6520cf221fb93e22d07657b921` | `cd0dbf13b3e5b99a2293a38e4e05029803350cad` | Merged; a repeated local native test exposed a pre-existing readiness race |
| Native test waits for listener, PR [#1624](https://github.com/GavinDu/Chat-Gateway/pull/1624) | `4c51ccb1804cc8cb445ddf6651be1858c68c748e` | `52e35250a621872dfdb2e4dfc9bd8334c8704324` | Merged; all final main release gates passed |

Earlier capture work is in PRs #1617–#1620; extension 0.3.5 selects the supported
unpartitioned ChatGPT capture profile. Unexpected returned partitions still fail
closed. This is not full partitioned-cookie replay support. Owner-reported Mac
acceptance for extension 0.3.1 was recorded on #1613/#1615: automatic native
messaging, automatic ChatGPT/Meta AI resync, no manual Sync clicks, and successful
bridge disable/re-enable. Later native observations identified extension 0.3.5.
PRs #1623/#1624 did not change extension files or require another extension reload.

## Testing

### Exact-head routine CI

All three routine jobs—`test`, `embedded-native`, `docker-build`—passed on:

- PR #1623 final feature head: [run 45830009310](https://github.com/GavinDu/Chat-Gateway/actions/runs/45830009310).
- PR #1623 merge: [run 45831035680](https://github.com/GavinDu/Chat-Gateway/actions/runs/45831035680).
- PR #1624 feature head: [run 45831943700](https://github.com/GavinDu/Chat-Gateway/actions/runs/45831943700).
- Final main `52e35250`: [run 45832906070](https://github.com/GavinDu/Chat-Gateway/actions/runs/45832906070).

The final main checks above apply to `52e35250`, not automatically to this later
handoff-only commit. Its review/CI state is recorded on its own pull request.
The optional manual ARM64 release job was not selected or claimed as passing.

### MCP full suite and Docker acceptance

Final main `52e35250` passed:

- 1,059 Rust tests: 20 library, 1,035 binary, one cookie snapshot and three provider fixtures.
- 137 Python tests, including 13 native bridge cases; the focused native suite also passed separately.
- The unchanged extension Node suite, formatting, Clippy with warnings denied, client-hook syntax and Docker helper checks.
- Actual `scripts/docker-main-e2e.sh` image build, binary/source provenance and runtime smoke.
- All nine MCP HTTP acceptance groups: build/health/request ID; offline readiness/models; API auth/malformed JSON; Host/Origin/role/browser controls; pairing/single-use; relay nonce replay; honest account-probe results; admin metrics/bounded request purpose; unpair revocation.
- Graceful exit zero and removal of each feature/main acceptance container and its four temporary fixture volumes.

The MCP main workspace was `/workspace/chat-gateway`; feature workspaces and
full tested SHAs are in [testing manifests](testing/). These HTTP fixtures used
empty synthetic sessions. They do not prove signed-in provider behavior or native
Linux browser/SSH acceptance. The native browser/SSH suite was not selected;
HTTP checks must not be relabeled as that suite.

### Failures retained and corrected

- The initial network classifier integration referenced unavailable `reqwest`.
  Root adapted it to the existing HTTP URI parser with an exact HTTPS authority gate.
- The next revision failed Clippy because `Atomic::fetch_update` is deprecated in
  the selected toolchain. The final feature uses `try_update`.
- On main `cd0dbf13`, the full Python suite passed but a separately repeated
  native test observed socket mode 0700 between `bind` and `chmod(0600)`/`listen`.
  Its local gate failed, so installation stopped. PR #1624 fixes the test's
  readiness predicate, retaining private-directory/socket permissions and the
  native framing, contender ownership and reload transaction checks. A controlled
  bound-but-not-listening regression covers the observed interval.
- A copied main-validation runner initially carried the previous expected SHA.
  It was stopped, its partial record retained, and validation rerun with an
  assertion against the actual expected main head. It was not used as passing evidence.

### Measurement tool checks

The committed operator snapshots have **15 offline regression tests**: six for
Meta AI aggregation, identity and proxy admission/snapshot behavior, plus nine for
ChatGPT content-free observation/network projection. Run from the repository root:

```sh
python3 -B -m unittest discover \
  -s docs/verification/runs/2026-11-26-metaai-chatgpt/tools/metaai -p 'test_*.py'
python3 -B -m unittest discover \
  -s docs/verification/runs/2026-11-26-metaai-chatgpt/tools/chatgpt -p 'test_*.py'
```

These checks do not send provider requests. The proxy regression starts 16
in-process mocked callers and proves exactly one forward plus 15 counted local
budget denials, with no final-snapshot overwrite. The 16 mocked calls are not
part of the real Meta AI or ChatGPT request counts.

## Live ChatGPT results and remaining budget

Selected scope: existing personal saved session, private copies inside Docker,
JSON/SSE, two-turn recall and disposable restart; maximum **12 gateway chat
attempts across this task**, concurrency one, at least five seconds between
starts. No automatic retries, load/soak, logout, crash tests, challenge bypass or
installed-service restart was selected. Stop the dependent chain on its first
failure. The shared private ledger is authoritative and must not be reset.

| Attempt | Revision/scenario | Outcome | Total seconds |
|---|---|---|---:|
| 1–3 | Earlier baseline/import checks | HTTP 502 before Send; no provider prompt sent | 1.678 / 1.525 / 1.877 |
| 4 | `1f082bb8`, fresh JSON | Exact response and durable turn passed | 50.920 |
| 5 | `1f082bb8`, independent fresh SSE | Stop/DONE and content passed; first content 33.341s | 50.442 |
| 6 | `1f082bb8`, distinct canonical replay | HTTP 502, no first content; chain stopped | 120.238 |
| 7 | `c9d9c252`, new distinct replay on fresh private page | Exact JSON and two durable canonical turns passed | 50.160 |
| 8 | `c9d9c252`, remember/ACK | HTTP 502; no assistant node/text in 251 polls; chain stopped | 120.204 |
| 9 | `52e35250`, new fresh JSON memory seed | HTTP 502; no assistant node/text in 259 polls; chain stopped | 120.104 |

**Nine used; three remain.** Failed prompts 6/8/9 have ambiguous remote outcomes;
do not resend them. Failure 8 was confirmed not locally committed; the older
canonical conversation retained two completed turns. No success/durability
claim is made for attempt 9. The old scenario chains are failed, and the current
round's 1,800-second window has expired; do not resume its runner as an active run.

### What the richer attempt-9 evidence establishes

- Session/account checks returned HTTP 200; account readiness and submission returned.
- One observed prepare POST returned 200 JSON. One observed conversation-submit
  POST returned 200 event-stream with **19,669 buffered bytes**, about **15.229s**
  after the gateway request started. The dispatch-to-callback timestamp gap was
  5.949s; this is not request-ID-attested pairing or incremental SSE timing.
- WebSocket metadata recorded 24 text frames, 102–2,807 bytes, no top-level
  `json_error` flags, and no known connect/stall errors. This is not full protocol validation.
- Browser polling still found zero assistant nodes/text through timeout.
- The collector reported no rejected, dropped or truncated observations.

The failure also occurs on a fresh page, so warm-page reuse alone is insufficient
as an explanation. Buffered bytes and HTTP 200 do not prove assistant text or a
valid terminal event. Body content was not retained/exported. Frontend stream/API
compatibility, an upstream SSE error/protocol variant and rendering/extraction
remain competing causes.

## Runtime and installation details

Read-only health observations at **2026-11-26 17:27 UTC**:

| Target | Running source | Enabled providers | State |
|---|---|---|---|
| Production `chat-gateway`, host 8080 | `391ad048c66cbfc2f267c49160cb2de3f40c9fa7` | Meta AI and ChatGPT | Process health `ok`; not restarted |
| Disposable `cg-chatgpt-epic1599`, MCP loopback 18940 | `52e35250a621872dfdb2e4dfc9bd8334c8704324` | ChatGPT only | Process health `ok`; last selected chat failed |

The disposable image is
`sha256:c18de9169debb0a0d5817f617512e7d0a8518f758c7b82edfe51cfbeb9d55e90`.
The host main checkout was fast-forwarded to `52e35250`; existing untracked backup
files were preserved. The installed production image/tag and its four original
volumes were not replaced. Process health is not proof of successful chat.

The disposable target retains its four `cg-chatgpt-epic1599-*` volumes. Failed
private pages were discarded before subsequent diagnostic rounds, while canonical
test history was preserved. Selected encrypted session/key copies were made
inside Docker from read-only production volumes. A fixed read-only native Chrome
account preflight verified session mode in 0.170s before the latest installation;
that historical probe is not fresh readiness for a future run and does not by
itself verify the exported snapshot. Private ledgers, provider state, raw logs and
an encrypted rollback snapshot remain outside version control. No unrelated
containers/volumes were pruned. Only explicitly verified unused task images were
removed for Docker disk space.

## Meta AI performance measurements

Actual OpenCode 2.4.0 CLI used the installed Chat-Gateway Meta AI provider for
scoped public-source coding, tests, reviews and analysis. Root reviewed/integrated
the contributions. The CLI agent had native tools disabled; this was not an
unrestricted autonomous repository-tool run. Each recorded job forwarded one
primary gateway attempt. All failures remain in the dataset.

For the 30 protocol-complete streams:

| Wire metric | Minimum seconds | Median seconds | Maximum seconds |
|---|---:|---:|---:|
| First content | 6.089 | 14.717 | 52.767 |
| First-to-last content | 3.695 | 8.453 | 53.473 |
| Last content to DONE | 15.310 | 15.936 | 17.749 |
| Request total | 29.809 | 41.024 | 89.740 |

Separate medians do not add into a representative total. CLI process duration is
recorded separately; TUI render/user-perceived latency was not measured. Tokens
are gateway estimates; character/delta counts are separate quantities. A normal
stop/DONE is not proof of valid code or valid JSON: malformed framing and lost
Python indentation required reviewed corrections on some contributions.

The export covers all **44 declared browser stages**, 1,339 nonzero stage windows
and 71,185 before/after metric-series observations. Costs over one second have
measured children or explicit opaque engine/network boundaries. Repeated
subsecond operations may sum to many seconds. Parent/child scopes overlap;
process-lifetime maximum gauges are not per-request maxima. See
[the stage catalog](measurements/stage-catalog.json) and
[per-call stage measurements](measurements/browser-stages.csv).

Important coverage limits:

- First four jobs lack per-call health identity; first 24 lack per-call image proof.
  Calls 25–36 have stable before/after image/source/health observations.
- Historical finalizers overwrote the after-metrics file; exact window skew is
  unmeasured. Raw surviving files and the coverage flag are retained. Call 35
  uses the corrected single-snapshot collector; call 36 also uses schema 3's
  separate local budget-denial counter and explicit input-size scope.
- Earlier local budget denials were not counted; do not infer zero retrospectively.
- Warm/cache state, true browser RSS and total observer CPU overhead were not measured.
  Container resource samples have an observed median interval around 0.5s, with
  a startup sampling gap. Shared production/build activity prevents causal attribution.
- Prometheus snapshot costs and final health/image collection are timed separately;
  those durations are not total instrumentation overhead.
- One old deadline job has no final metrics snapshot; the first resource job has
  no usable samples. Missing values are not zero.

### Improvement priorities

The [measured plan](measurements/improvement-plan.json), also tracked in
[#1602](https://github.com/GavinDu/Chat-Gateway/issues/1602), ranks verified
response-specific completion evidence first (reuse
[#136](https://github.com/GavinDu/Chat-Gateway/issues/136)). Much of the observed
~16s normal tail is a conditional recovery opportunity. Preserve long pauses,
late fragments, stale controls, identical replies, origin and cancellation guards;
do not merely shorten the conservative timer.

Even removing that tail does not establish a five-second total. Measure native
preparation/fetch and remote wait boundaries, output-generation floors, source
fidelity, warm state and observer/resource costs in separately bounded output
classes. The current heterogeneous development cohort is not a p95/reliability
SLO or a controlled before/after comparison. No sub-five-second result is claimed.

## Evidence and privacy

[The archive](measurements/telemetry-export.zip) contains consolidated reports,
all available metric-series deltas and explicit per-job numeric/provenance files,
Prometheus snapshots and projected resource samples. It excludes prompts, raw
conversation transcripts, raw CLI stdout/stderr, SDK configuration/cache,
credential/cookie/vault material and private conversation stores. Request IDs are
HTTP correlation IDs, not provider account/thread IDs.

[The manifest](manifest.json) and [transfer verification](transfer-verification.json)
record exact source-file hashes. CSV views retain their captured CRLF endings;
the directory attribute preserves those bytes and their hashes. [Testing manifests](testing/) preserve successful
checks and the main gate failure. Operator source snapshots under `tools/` retain
the recorded local paths and experiment assumptions; they are not general
installation instructions. Keep live credentials outside this tree.

## Next implementation steps

1. Reproduce the pinned native `Response.body` behavior offline: it throws after
   reader acquisition. Evaluate extending the existing Meta AI-only stable-getter
   compatibility to ChatGPT, with exact HTTPS/provider/shape gates, unchanged
   reader locks/disturbance/abort/cancel semantics, and no replay of consumed bytes.
   This defect is concrete; its causal role in the observed timeout is unproven.
2. Add bounded content-free upstream SSE line metadata if needed to distinguish
   known assistant envelopes, errors and unrecognized framing. Do not log or feed
   provider bodies, user content, account/thread IDs or credentials to Meta AI.
   A planned coding prompt exists locally; no implementation was produced by the
   approval-blocked call.
3. Follow the normal feature CI/MCP candidate, merge, exact-main and applicable
   installation gates for any runtime fix. Existing green checks cannot attest
   a future revision. Preserve the selected production-restart exclusion.
4. Before another live request, create a fresh resolved run record, attest its
   exact build and readiness, and calculate the selected scenarios against the
   **three remaining** requests. Keep concurrency one, five-second spacing,
   stop-on-first-failure and no automatic retries. Obtain an explicit additional
   budget only if the concrete remaining validation cannot fit; never reset the ledger.
5. Update epic [#1599](https://github.com/GavinDu/Chat-Gateway/issues/1599),
   #1621 and installed acceptance [#1294](https://github.com/GavinDu/Chat-Gateway/issues/1294)
   with actual results. Do not close installed acceptance from fixture or partial
   disposable success. Continue using Meta AI for substantive work, retaining
   every attempt, quality correction and measurement gap.
