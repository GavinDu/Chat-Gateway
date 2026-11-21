# Provider verification implementation roadmap

This backlog implements [version 1 of the standard](PROVIDER_VERIFICATION_STANDARD.md).
It records the audit of `ede53339d3f48971c3db253fb96b64a2e71898f5` and keeps
observed gaps distinct from unmeasured behavior. Required owner inputs are
listed in [the input checklist](PROVIDER_VERIFICATION_INPUTS.md).

## Canonical runner and evidence

- Make acceptance use the actual production completion endpoint and terminal
  success. The current adapter diagnostic stops after first changed text and
  uses a separate send path; it cannot certify completed delivery.
- Input preflight is implemented in `scripts/provider_verification_inputs.py`.
  It lists missing explicit inputs and produces blocked stage records without
  network/session access. Its complete-input exit is not live evidence. Full
  result validation and the canonical live executor remain separate work.
- Add fixed stage/reason/duration/submission-state events for all providers.
  Current request-failure diagnostics cover only ChatGPT/Meta AI and coarse
  stages. Preserve request correlation without metric-cardinality growth.
- Check catalog parity. The adapter/extension catalogs contain nine providers;
  the installed smoke runner currently omits Manus. Record task capabilities
  instead of forcing every adapter through identical chat assumptions.

## Correctness and recoverability

- Completion rendering now creates one ID and timestamp per response, with
  real-router regression coverage for repeated Meta AI and ChatGPT JSON/SSE
  turns in the same conversation. This replaces the audited conversation-prefix
  IDs and per-chunk timestamps; exact-head release validation is still required.
- Expose rotated-session save outcome separately from successful reply delivery.
  The current worker logs save failure after forwarding completion. Add
  fault/restart checks and recovery without resending the delivered prompt.
- Add random-marker memory recall on follow-up and after restart. Current
  independent `OK` and `TWO` prompts demonstrate distinct answers, not recall.
- Expand latency buckets past ten seconds and add useful phase measurements.
  Historical Meta AI requests took roughly 20–22 seconds.

## Meta AI live certification

Use Meta AI as the first requested production focus after explicit selection of
the live model, environment, account/session, scenarios, limits and targets.
Verify fresh capture, JSON/SSE, freshness, memory, persistence/restart and
selected faults before the approved load/soak plan. Authenticated-account proof
remains unsupported until a reviewed first-party evidence contract exists.

Eight prior real requests across saved-state restart are functional backend
evidence. They do not substitute for fresh Chrome capture, an availability
target, account proof or a sustained resource/recovery measurement. Record
future results in the live acceptance register with sanitized evidence links.

## Delivery gates

Implement changes on a feature branch and PR. Pass exact-head CI and applicable
isolated MCP acceptance before merge. Repeat applicable checks on the merged
release. This roadmap does not authorize a live run, production restart,
installation or recurring job; use the resolved run inputs and existing
development workflow for those actions.
