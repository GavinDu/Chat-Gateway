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
   installing a release. This diagnostic change does not fix the 502 itself.

Sandbox capability preflight at /workspace/chat-gateway: Linux; node, python3,
and cargo present; ssh absent; no configured executable Chromium path.
Browser/SSH acceptance: NOT RUN — missing SSH; Chromium launch/sandbox/runtime
prerequisites have not been established. HTTP-only tests cannot substitute.
No candidate image, merge, or new deployment was performed in this investigation.
