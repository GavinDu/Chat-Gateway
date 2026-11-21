# Provider verification standard

Version 1 defines the evidence required to verify a provider from Chrome session
capture through an Obscura completion delivered to a client. It applies to
ChatGPT, Claude, Gemini, Grok, Meta AI, Mistral, Perplexity, Qwen and Manus.
Provider-specific authentication, task and completion behavior must be recorded
explicitly. A working adapter does not by itself establish production readiness.

## Run inputs and scope

Complete [the input checklist](PROVIDER_VERIFICATION_INPUTS.md) before execution.
Use [the blank input record](verification/run-inputs.template.json) to record the
answers. `null` means unanswered. No provider, model, account, environment,
permission, request budget, performance target or schedule is implied by this
standard. Missing required inputs block the affected work.

The input record is non-secret. Passwords, session cookies, tokens, account
identities and vault keys belong in their existing private credential stores.
The owner signs in on the official provider site; the extension obtains reusable
session material after login, without collecting the username or password.

## Evidence and outcomes

Record a result for every selected provider, scenario and stage. Use `pass`,
`fail`, `blocked` or `unsupported`, together with a fixed reason, UTC timestamp,
duration and evidence reference. A stage not attempted is `blocked` with an
explicit reason. Preserve each failed attempt; a later retry is a new attempt.

Distinguish `fixture`, `live` and `code_review` evidence. A fixture pass cannot
satisfy a live criterion. Previous runs remain historical evidence for their
recorded commit, image and session source. They do not establish a new capture
or current account state. A required unsupported capability blocks promotion
unless the owner explicitly accepts a reduced claim; it never becomes a pass.

Bind live results to the actual runtime commit, image digest where applicable,
extension revision, selected provider/model, scenario and opaque run/request
IDs. Pin the provider so fallback cannot conceal a failed adapter. Confirm the
runtime identity before sending a prompt.

Use [the result schema](verification/result.schema.json) for the result envelope.
The schema describes data structure; it does not execute checks, establish
evidence truth or automatically redact arbitrary strings. Reports must contain
only fixed categories, opaque test IDs, sanitized artifact references and
bounded measurements. Keep credentials, credential hashes, account data,
private URLs, prompts and replies out of shared reports and metric labels.

## Stage acceptance criteria

| Stage | Required evidence | Automatic positive and negative checks |
| --- | --- | --- |
| `1.capture` | Owner-selected Chrome profile and linked provider; permitted cookie scope and metadata preserved after manual login. | Permission denial/revocation, empty or expired capture, Domain versus host-only, path, Secure, HttpOnly, SameSite, expiry, logout/relink and unsupported partitions. |
| `1.persist_activate` | Encrypted snapshot saved, Obscura activation observed, and durability recorded separately. An upload or reply alone does not prove durable activation. | Capture/relay/vault/replay fidelity, scope rotation, corrupt vault, activation timeout, interrupted write and preservation of the last valid state. |
| `2.1.fetch` | Expected first-party origin/route and actionable composer; account-verification capability and result reported independently. | Cold/warm navigation, off-origin redirects, HTTP 200 login pages, expired credentials, document/script 403, blocked assets, challenge and network timeout. |
| `2.2.fill` | Intended text remains visibly rendered in exactly one actionable composer after framework processing. | Unicode, multiline text, emoji, quotes, long input, rerenders, hidden/duplicate editors, disabled inputs and draft replacement. |
| `2.3.submit` | One dispatch and a fresh matching user turn or reviewed provider task acknowledgement. | Disabled Send, lost acknowledgement, duplicate activation, disconnects while queued/typing/after dispatch and timeout after acceptance. |
| `2.4.extract` | Current first-party assistant response, preserved content under the selected format policy, and reviewed terminal evidence. | Old or identical replies, blank roots, hidden clones, code/lists/links, Unicode, slow pauses, DOM replacement, truncation and network failure. |
| `2.5.envelope` | Correct JSON/SSE schema and provider/model/conversation; unique completion ID per request and stable ID/timestamp within one stream. | Escaping, consecutive turns, concurrent requests, usage-estimation marking, malformed events and partial/persistence errors. |
| `2.6.deliver` | Client receives a complete valid JSON result or SSE successful terminal event followed by `[DONE]`; incomplete turns are not committed as success. | Socket drop, slow reader, deadlines, bounded buffering, cancellation, permit release, store failure and client-side content checks. |
| `recovery` | Selected state transitions preserve the flow and documented durability boundaries. | Random-marker recall on follow-up and after restart; crash, session rotation, logout/relink, refresh while busy, isolation, bounded queue/load and approved soak. |

Submission state is `not_sent`, `acknowledged` or `outcome_unknown`. After an
uncertain dispatch, stop or reconcile using reviewed provider evidence. Do not
blindly resend. A click, HTTP 200, first token, stable old text or `[DONE]` alone
does not prove a completed reply.

Account evidence is independent of reply readiness. If a provider has no reviewed
first-party account contract, report authentication as `unsupported` or
`unknown`, as appropriate. Successful anonymous or account-unknown replies must
not be labeled authenticated-account verification. Stop for a provider challenge
and require the owner's normal manual resolution.

## Verification levels

1. **Fixture CI:** exercise all provider stage contracts, the production router,
   JSON/SSE reducers, schema/redaction and catalog parity without live accounts.
2. **Release sandbox:** test the exact packaged image with disposable state,
   relay/auth/Origin/pairing guards, applicable browser fixtures, failure
   injection, shutdown and cleanup. Follow the existing capability preflight.
3. **Live certification:** use only the selected accounts, providers, models,
   scenarios and approved budgets. Verify through the actual production endpoint.
4. **Operational monitoring:** distinguish read-only health from a real completion
   canary. A schedule, account, budget, alert destination and retention policy
   require explicit run inputs; this document creates no schedule.

The legacy `--test-adapter` command and existing smoke tools provide partial
coverage. They do not yet implement every requirement here. Track missing
capabilities in [the implementation roadmap](PROVIDER_VERIFICATION_ROADMAP.md).

## Production promotion

Apply [the development workflow](DEVELOPMENT_WORKFLOW.md) and
[release checklist](TESTING_CHECKLIST.md) to the exact feature and merged-main
commits. Keep the nine-provider live acceptance register separate from synthetic
CI results. Missing required capabilities or evidence block the corresponding
promotion gate. Never install a feature build or bypass a failed required gate.

Require zero observed credential leakage, cross-account/conversation content,
stale replies accepted as new, partial replies reported successful, duplicate
dispatch in selected fault scenarios, or unreported session-persistence failure.

Sample counts, soak duration, traffic/concurrency limits, success-rate and latency
targets must be supplied by the owner. Baseline measurement can establish future
targets but cannot claim to have met an unset target. Save cold/warm samples,
every failure, queue/first-token/total timing and resource measurements separately.

Meta AI is the first requested production focus. Its historical eight-request
backend run covered JSON, SSE and distinct replies across saved-state restart.
Fresh extension capture, authenticated-account proof, true memory recall,
failure recovery and sustained reliability each require their own evidence.

## Ownership and cleanup

Inventory test-owned containers, processes, volumes, sessions and keys before
mutating them. Restart, logout, fault injection and cleanup must stay within the
approved ownership boundary. Preserve original provider state and unrelated
services. Remove disposable credentials and state after a run, and retain only
the approved sanitized evidence. Report cleanup failures explicitly.

## Change control

Store the standard, input record and result-schema version with each report.
Changes to pass criteria require a version change and review. Older evidence
keeps its original version. Project copies must identify the repository commit
and preserve the corresponding normative content. Declare any navigation-link
rewrites in a snapshot manifest and retain hashes of source and copied files.
