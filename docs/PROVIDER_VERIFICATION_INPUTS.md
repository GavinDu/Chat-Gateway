# Provider verification inputs

Complete these steps for a new verification run. An unanswered field remains
unset and blocks dependent work. Use explicit `false`, `none` or `not applicable`
with a reason when an action or capability is excluded. Previous consent still
applies to its original scope; this round requires explicit run-specific choices.

## Step 1 Select the deliverable

State whether the work includes persisting the standard, implementing missing
verification capabilities and executing selected checks. Record the storage
destinations. Record the explicit selections in the run input record. Choosing
implementation and execution does not select live providers, accounts or
installation. Dependent work waits for its required inputs.

## Step 2 Select providers and outputs

List each selected provider and exact gateway model alias. The provider catalog
contains ChatGPT, Claude, Gemini, Grok, Meta AI, Mistral, Perplexity, Qwen and
Manus; catalog membership is not selection. Choose JSON, SSE or both, and define
the expected content policy: plain text, Markdown or another explicit contract.
For a task-oriented provider, specify the accepted task-completion evidence.

## Step 3 Identify the target runtime

Supply the gateway origin and Docker/native/remote deployment type. Choose an
existing build or explicitly request a new isolated merged-main build. Identify
the expected commit and image digest before execution. A build-created digest
may be recorded after building, but no live request may precede attestation.

Identify the configuration by a non-secret revision/label. Specify whether test
resources are disposable and whether the installed service may be touched.
Remote mode must already satisfy the repository's documented network controls;
testing does not authorize exposing a local control plane.

## Step 4 Prepare Chrome and the session

State whether Chrome is available, identify its profile and the extension build
with non-sensitive labels, and choose fresh capture or an existing saved session.
Choose a dedicated test account or explicitly authorize the selected personal
session through its local private mechanism. Do not send account identifiers,
passwords, tokens, cookie values, vault keys or private thread URLs in chat.

For fresh capture, the owner signs in on the official site, completes ordinary
verification, grants the selected provider permissions and links the session.
Record capture, persistence and activation separately. If only an existing
snapshot is available, fresh-capture acceptance remains blocked.

## Step 5 Select scenarios and permissions

Explicitly include or exclude normal replies, fresh capture, memory recall,
graceful restart, crash recovery, logout/relink, refresh while busy, fault
injection, concurrency and soak. Record the exact resource ownership boundary.

Answer separately for live requests, temporary private session copies, restart
of disposable resources, restart of installed resources, logout/relink, fault
injection, load testing, installation/deployment and recurring monitoring.
Permission for one action is not permission for the others.

## Step 6 Set budgets and acceptance targets

Supply the maximum provider requests, minimum interval, concurrency, queue and
response timeouts, maximum wall-clock duration and any daily/cost cap. Count
retries, setup prompts, memory turns and canaries against the approved budget.
State what stops a run and whether a safe retry is permitted before dispatch.

Choose baseline measurement only or explicit success-rate, queue, first-token,
total-latency and resource-stability thresholds. Set sample count and soak
duration for any reliability claim. Earlier suggestions of 50–100 turns or
24–48 hours are proposals, not selected values.

## Step 7 Select evidence handling

Choose the local output directory, report retention and allowed sharing. Keep
secret stores separate from the versioned input record. Identify who will
receive results; recurring alerts require a separately selected destination and
schedule. Do not put private identifiers in the shared evidence.

## Step 8 Resolve and execute

Copy [the input template](verification/run-inputs.template.json) to a private
working location outside the repository. Fill the fields and mark exclusions
explicitly. The operator returns a resolved run plan listing the target,
selected stages, permissions, budget, stop conditions and unresolved inputs.
Resolve every required input before dependent implementation or live execution.
Read-only inspection and preparation that do not depend on those answers can
continue. Material scope changes require updated inputs.

Run the implemented input preflight from the repository root:

```sh
python3 -B scripts/provider_verification_inputs.py \
  --inputs /private/location/run-inputs.json \
  --report /private/location/input-preflight-results.json
```

It prints selected providers and unresolved field names without reading sessions,
starting a runtime or making network requests. Exit 2 means inputs are missing
or invalid; exit 0 means the checked input fields are complete. Both outcomes
leave every execution stage blocked until an executor observes it. Expected
commit/digest values are never reported as observed runtime identity. Lifecycle
and load executors must additionally calculate the cost of their selected
scenarios before execution; the current preflight costs normal replies and
two-turn memory recall only.

The runner then performs preflight, attests identity, executes selected checks,
records every outcome and cleans up only test-owned resources. End with passed,
failed, blocked and unsupported stages, exact build identity, budget used,
retained evidence and cleanup status. Follow the existing PR and release gates
for implementation changes; installation remains a separately selected action.

## Short reply form

```text
Providers and exact models:
Outputs and content policy:
Target runtime and gateway origin:
Existing build or isolated merged-main build:
Expected commit and image digest or build-attestation plan:
Configuration label and resource ownership boundary:
Chrome available, profile label and extension build:
Session source and local account authorization method:
Included scenarios:
Excluded scenarios:
Live requests and temporary session-copy permissions:
Disposable restart, installed restart and logout/relink permissions:
Fault injection and load-test permissions:
Maximum requests, minimum interval and concurrency:
Queue/response timeouts and maximum duration:
Daily/cost cap and stop/retry policy:
Baseline only or explicit acceptance thresholds:
Sample count and soak duration:
Evidence location, retention and sharing:
Install/deploy included:
Recurring monitoring included, schedule and destination:
```
