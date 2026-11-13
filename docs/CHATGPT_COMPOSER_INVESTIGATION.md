# ChatGPT composer investigation — 2026-11-13

## Reproduced deployment

Base commit: `042ea5d205a3b1761ceb52e1b5193d1e824821a0`.
The installed Docker gateway is healthy. The relay is in local mode without
pairing. Existing config.example.yaml edits enabling ChatGPT and Mistral were
preserved and excluded from the investigation commit.

Both independent non-streaming requests (`model=chatgpt` and `model=mistral`,
message `Reply OK`) returned HTTP 502 / provider_unavailable.

ChatGPT preflight reports complete document readiness, expected origin, no
login redirect or visible challenge, no configured composer, and an actionable
generic editor. The adapter diagnostic identifies an editable textbox but no
configured input, send, or response selectors. Restored cookies alone and the
legacy session_ok flag do not prove authentication.

Mistral separately times out at composer readiness: blocker=undetermined,
no observed script HTTP 403. Do not classify this as an asset policy denial.

## Comparison and limits

A temporary session-free Mac browser probe using the same preload scripts
found a visible mobile-composer-prompt textarea inside a form, with a known
sign-in control present and no known account control. Both immediate inspection
and a subsequent rendering-task settling probe produced that result. This is
not the saved-session Docker page and cannot justify broadening its selectors.
The temporary probe was removed.

The running gateway's login inspection routes reject inspection with
owner_authorization_required because no admin API key is configured. No
credentials were changed and no authorization checks were bypassed. A normal
browser account login, relay refresh, and a screenshot of the actual failing
page have not been verified/captured.

## Diagnostic change

Log fixed structural evidence at ChatGPT/Meta AI composer failure and expose it
in adapter reports: actionable editor categories, visible editable controls in
forms/dialogs, and presence of known sign-in/account controls. No page text,
attribute values, cookies, or prompt values are logged. These are observations,
not proof of authentication and not new send-path fallback selectors.

Adapter reports now explicitly report authentication=unverified unless an
explicit login redirect establishes login_required. The legacy session_ok
field remains compatible and is documented as a redirect check only.

A fixture checks hidden/disabled controls, form/dialog classification, redaction
in serialized/logged evidence, and that no generic send fallback is introduced.

## Next diagnosis and release gates

1. Pass CI on this branch. Run isolated fixture E2E only after green CI, using
   disposable sessions and a commit-specific image per DEVELOPMENT_WORKFLOW.md.
2. Inspect the actual failing page through owner-authorized browser inspection
   and compare editor structure with the configured selectors. Capture a
   screenshot privately; do not add account screenshots or HTML to Git.
3. Verify normal browser authentication and refresh the provider relay link.
   Do not treat cookie presence, absence of a login URL, or account-control
   presence alone as proof of a usable authenticated session.
4. If a real composer with changed markup is confirmed, add a provider-specific
   selector and a sanitized regression fixture. If rendering is delayed, fix
   readiness using a bounded actionable-editor wait. Do not select arbitrary
   textboxes, searches, or dialog fields.
5. Require actual HTTP 200 with a nonempty assistant response for each provider.
6. Complete sandbox, merge, merged-main E2E, and installation gates before
   installing a release. Successful real-provider completion is still required; diagnostic and fixture success alone is insufficient.

Sandbox capability preflight at /workspace/chat-gateway: Linux; node, python3,
and cargo present; ssh absent; no configured executable Chromium path.
Browser/SSH acceptance: NOT RUN — missing SSH; Chromium launch/sandbox/runtime
prerequisites have not been established. HTTP-only tests cannot substitute.
The user subsequently explicitly authorized committing, restarting, and testing
feature changes in a closed loop. Feature builds were installed for that live
diagnostic loop, preserving the production volumes. No merge or merged-main
release validation has been performed. This is not a validated main release.

## Closed-loop findings and fixes

- An existing private debug capture identified the live ChatGPT form as
  `form[data-chatgpt-composer]` with a ProseMirror editable textbox and a
  form-scoped Send control. Add those narrow selectors and reject ambiguous or
  dialog/search form candidates before production sends.
- The pinned Obscura bootstrap exposes a generic value property on DIVs and
  implements execCommand as a no-op. Use element type rather than property
  presence when reading/writing composer text. Offer cancellable plain-text
  paste to controlled editors before the DOM fallback; do not forge trust.
- An isolated encrypted session copy now finds the ChatGPT editor, retains the
  single-line diagnostic prompt, and clicks Send. No response token arrived
  within 45 seconds; network metadata alone does not establish authentication
  or successful completion.
- Real requests contain a multiline quoted transcript. Boolean-only evidence
  established that the live editor discarded the final generated separator
  whitespace. Remove only the generated trailing newline outside the transcript
  and personal-memory wrappers; preserve user whitespace inside each wrapper.
  Draft retention and send guards remain exact, without trimming user content.
- Startup and relay refresh used unbounded initial document navigation while
  holding the shared browser operation lock. Bound the full restore/navigation/
  reload sequence to 30 seconds. Long generation and provider failures remain
  separate from this navigation budget.
- The extension treated persisted-but-not-activated sessions as failed uploads
  and could remove first-link consent. Recognize only the gateway's fixed
  session_activation_timeout/session_refresh_failed codes as saved activation
  pending. Preserve consent, invalidate stale status, do not mark snapshot
  activation successful, and do not immediately replay the upload.

## Provider coverage

All eight enabled providers were tested independently using configured model
aliases (Meta AI uses meta-ai, not metaai). Intermediate full-stack tests still
returned 502 for all eight. Observations: Meta AI document/API HTTP 403; Mistral
unavailable composer with DOM errors and no observed script HTTP 403; Qwen and
Perplexity response timeout; Claude module-worker and input failures; Gemini
and Grok input failures. These are observations, not a blanket authentication
or anti-automation diagnosis. Manus is disabled and was not enabled.

The final generated-newline correction requires another real-provider sweep.
Record exact final results on the PR; do not infer success from the tests below.

## Local checks for the combined changes

- 87 extension tests passed, including saved-activation-pending link retention.
- 36 composer tests passed, including controlled paste without document
  destruction, stale DIV value handling, and ambiguous/dialog editor rejection.
- All three provider fixture integration tests passed.
- Full Rust suite: 650 passed, one failed because the existing embedded-login
  profile test requires Linux /dev/shm, absent on this Mac. The test was not
  skipped or weakened. Required Linux CI remains authoritative for that case.
- Formatting, diff checks, and clippy passed; existing dependency warnings remain.
- Chrome live extension console was attempted twice but the desktop remained
  locked, including after the user opened SSH. No live extension console or
  extension reload has been verified. Source tests do not establish that the
  installed extension is running the edited files.
