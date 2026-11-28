# OpenCode failure cohort — offline evidence mode (#1649)

Use the existing installed smoke runner, not a second live request engine:

```sh
python3 scripts/installed_provider_smoke.py --validate-opencode-cohorts /path/to/redacted-cohorts.json
python3 -m unittest discover -s scripts -p test_installed_provider_smoke.py
```

The mode is offline only. It rejects combination with `--execute` or `--repeat`. A zero exit status means schema and consistency validation, not successful recovery, verified source identity, or permission to send another request.

## Collection protocol

Obtain explicit owner authorization for the selected disposable service and a fixed request budget. Use ChatGPT or Meta AI only; keep all other providers deferred. Do not change production credentials or sessions. Do not replay an ambiguous submission; stop on ambiguity, challenge, rate limit or budget exhaustion. A subsequent independent synthetic request needs its own authorization and request ID.

For each independently reproduced failure, record one named intervention and its observed result, including untested controls as skipped. Suggested order: ordinary retry, delay matched to restart duration, restart only the OpenCode client while retaining the gateway and helper, recreate only client transport, direct API control. Repeat separately where authorized. Record exact local OpenCode, helper and gateway versions, process identities/uptime, configuration, bounded workload and timing in the owner's private evidence; never copy raw prompts, responses, credentials, URLs, process arguments or session handles into the manifest.

## Input schema v1

Top-level keys: `schema_version` (1), `provider` (chatgpt or metaai), `source_sha` (40 lowercase hex from running build health, not merely a checkout), `rows` (1–100 entries). Every control must appear at least once, observed or skipped; first appearances follow the listed order, and subsequent independent repetitions may appear later. The manifest has no permission or execution fields.

Each row has a unique random canonical UUID `cohort_id`, one `control` from the ordered names below, and `status` (observed or skipped). A skipped row has only these and `skip_reason` from: not_authorized, not_supported, not_reproduced, ambiguous_submission, rate_limited, challenge, budget_exhausted, confounded.

Observed rows additionally contain:

- `baseline_request_id` and `result_request_id`: distinct canonical non-nil UUIDs from actual gateway response headers, unique across the manifest. If unavailable, record an unobserved/skipped variant rather than invent a request ID.
- `before` and `after`: dictionaries with exactly client, gateway, helper, transport, workload, history, model, config; locally generated random UUID aliases for observed state identities, stable only within the evidence collection. Reuse an alias only for an observed equivalent state. Never hash account secrets, URLs or conversation handles into aliases.
- `delay_ms`: elapsed gap before intervention observation; `successes`: number of successful subsequent synthetic requests; `recurrence_ms`: time to renewed failure from intervention, or null when not observed. Null is not evidence of durable recovery.
- `outcome`: recovered, failed, or not_observed. A recovered observation requires at least one successful request. Record actual successes/time to recurrence, not speculative values.

| Control | Expected changed dimensions |
| --- | --- |
| ordinary_retry | none |
| matched_delay | none |
| client_restart | client, transport |
| transport_reset | transport |
| direct_api | client, transport |

The expected changes are conservative projections, not evidence of causation. Client restart necessarily changes transport in this model; direct API must retain equivalent workload and logical history. Unknown state identities or unsupported observations are skipped, never guessed. Additional or missing changes make the row confounded (`isolated: false`). Matching delays within 250 ms is an explicitly chosen initial analysis tolerance, not a production requirement. Each restart must have a distinct comparable delay control with matching workload, history, model and configuration aliases. The report omits raw state alias maps, emits only validated correlation UUIDs, fixed labels and bounded numeric evidence, and always states `live_verified: false` and zero sent provider requests. It does not attest that the supplied observations were measured. This slice does not automate OpenCode or prove #1649/#1652 acceptance.

## Remaining acceptance

Join actual gateway request/attempt observations from #1650, capture process/build versions and uptime from owner-local measurements, preserve unknown fields and repeat independent baseline/intervention cohorts. Do not claim recovery from a single success or unverified evidence. The current offline mode only validates a compact manifest; the authorized live isolation sequence remains separate.
