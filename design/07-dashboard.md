# Chapter 7 — Dashboard

The dashboard is served at `/` (with dashboard API routes below `/dashboard/api/`).
It displays configured Qwen and Meta AI provider state, request activity, effective
model catalogs, priorities, and session timestamps. Each provider card distinguishes the
editable configured aliases from provider-page discoveries and shows the catalog's last
successful refresh or error state.

## Manual login

The Login action opens an isolated browser and displays its current page inside the
dashboard modal. The same surface follows provider, OAuth, consent, and verification
redirects. Users can click, scroll, type, paste, and use navigation keys before saving
or cancelling the session. Saving captures cookies and localStorage from the temporary
profile and closes the browser. Frames are painted onto a stable canvas so polling does
not replace or reload the displayed element. Mobile users can focus a provider field,
then use a transient secure typing relay with Next and Enter controls.

The browser bridge serves compressed viewport frames and validated input events for a
single random login job identifier. It never returns a CDP/debugger address, DOM,
cookies, or localStorage. Redirect labels omit paths, queries, and fragments.
Authentication input and frames are not logged or persisted. Closing the modal or
dashboard cancels the browser, and an inactivity lease handles abrupt disconnects.
The main browser pool may separately use configured persistent per-provider profiles
without profile-lock conflicts.

## Configuration and access

Provider settings are saved through dashboard mutation routes. Changes to model aliases
can be applied in memory; priority changes require restart to rebuild routing order.
When `server.dashboard_secret` is set, mutation and remote-login routes require its
bearer token. Frame responses use `no-store`.

Dashboard login is manual only. There are no auto-authentication, magic-link, IMAP, or
CAPTCHA relay/solver endpoints or flows.