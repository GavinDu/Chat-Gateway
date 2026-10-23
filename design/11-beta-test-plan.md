# Chapter 11 — Beta Test Plan

This plan covers the supported Go Proxy product surface: the OpenAI-compatible API,
the dashboard, manual session capture, routing, and the two production adapters:
Qwen and Meta AI.

## 11.1 Beta goals

The beta should answer four questions:

1. Does the service start cleanly and expose only the supported providers?
2. Can a user inspect provider state and complete the manual-login workflow without
   giving the proxy a raw password?
3. Does the OpenAI-compatible API validate requests, route models, stream responses,
   and preserve conversation IDs?
4. Do authenticated Qwen and Meta AI sessions work correctly and within a measured
   latency range through their current
   Rod/CDP selector paths?

The first three questions can be tested without provider credentials. The fourth
requires a tester to authenticate manually in a headed browser. Credentials must
never be pasted into chat, committed to the repository, or sent to an API endpoint.

## 11.2 Test environment

Use a disposable development checkout with:

- Go 1.22 or newer
- Chrome or Chromium available on the host
- `config.yaml` copied from `config.example.yaml`
- The Go Proxy workflow running on port 8080
- No public exposure of the proxy during testing

Start the proxy with:

```bash
go build -o go-proxy .
./go-proxy --config config.yaml
```

The dashboard is available at `http://localhost:8080/`. The API base URL is the
same host.

Before each beta cycle, record the commit/checkpoint, operating system, browser
version, and whether each provider has an authenticated session. Do not record
cookies, localStorage values, profile paths containing personal information, or
provider passwords.

## 11.3 Entry criteria

- `go test ./...`, `go vet ./...`, and `go build` pass.
- `/health` responds with HTTP 200.
- The dashboard loads in a fresh browser context.
- The tester has a way to manually authenticate to any provider included in the
  authenticated portion of the beta through the dashboard's remote browser.
- A rollback/checkpoint is available before any session or configuration changes.

## 11.4 Functional test cases

### BT-01 — Service health and provider catalog

**Steps**

1. Start the proxy.
2. Request `GET /health`.
3. Request `GET /v1/models`.
4. Request `GET /dashboard/api/providers`.

**Expected**

- Health returns HTTP 200 and a healthy service status.
- Only Qwen and Meta AI appear in provider state.
- Model aliases are limited to the configured Qwen and Meta AI aliases.
- Dashboard provider priorities match the two configured values.
- No inactive provider is registered at runtime.

### BT-02 — Dashboard presentation

**Steps**

1. Open `/` in a fresh browser context.
2. Confirm the page redirects or renders the dashboard.
3. Inspect all provider cards and the request log.
4. Resize to a narrow viewport and reload.

**Expected**

- The page is not blank and remains usable after reload.
- Exactly two provider cards are visible.
- Cards show provider state, model aliases, priority, and session information.
- No CAPTCHA overlay, password field, mailbox login, or automated-login control
  is present.
- The request log empty state is readable when there are no requests.
- No element is clipped or inaccessible at the narrow viewport.

### BT-03 — Manual-login modal before browser launch

**Steps**

1. Open Log In for Qwen (repeat for Meta AI if time permits).
2. Inspect the initial modal.
3. Close it without starting a login.

**Expected**

- The modal identifies the selected provider.
- It explains that authentication is completed in a temporary browser displayed in
  the modal.
- The initial actions are Close and Start Login.
- No raw-password input is present.
- Closing the modal leaves the dashboard responsive and unchanged.

### BT-04 — Manual-login lifecycle and cancellation

**Steps**

1. Open a provider's Log In modal.
2. Click Start Login.
3. Wait up to 30 seconds for a browser frame or a readable error state.
4. If ready, confirm the provider page, Save Session, and Cancel are visible.
5. Click a harmless page location, scroll, focus the view, and press Tab.
6. If the provider redirects to its login or OAuth host, confirm the frame follows it
   and the displayed location contains no query string.
7. Without entering credentials, click Cancel.
8. Close the modal and reload the dashboard.

**Expected**

- The temporary browser opens in the modal, or a readable error/fallback is shown.
- Click, scroll, and keyboard input reach the current page.
- A waiting state exposes Save Session and Cancel.
- Cancellation closes the login browser and does not leave a job stuck in
  `starting` or `waiting`.
- A cancellation during browser startup does not later become `waiting`.
- Frames and authentication input are not persisted or included in logs.
- The dashboard remains responsive and the provider remains `no_session` when no
  session was saved.

### BT-05 — Session-file detection

**Steps**

1. Start with no session file for a selected provider.
2. Create a disposable, non-authenticated session fixture containing empty
   cookies and localStorage.
3. Confirm the provider changes to `ready`.
4. Remove the fixture.
5. Confirm the provider returns to `no_session`.

**Expected**

- File creation and removal are reflected without restarting the proxy.
- No real provider credential or cookie is used for this test.
- The fixture is removed after the test.

For a real beta account, replace the fixture with the manual Save Session flow
and verify that the resulting file is private and contains no raw password.

### BT-06 — API validation and no-session behavior

**Steps**

1. POST an invalid chat request with no `messages`.
2. POST a request for an unknown model.
3. POST a valid request while all providers have no authenticated session.
4. Repeat with both `stream: false` and `stream: true`.

**Expected**

- Invalid requests return HTTP 400 with a useful error.
- Unknown models return HTTP 400 with `model_not_found` rather than silently
  mapping to another provider.
- A valid request with no available provider returns HTTP 503.
- The server remains healthy after each error.
- Error responses do not expose cookies, localStorage, passwords, or browser
  debugging data.

### BT-07 — Authenticated provider smoke test

Run this case separately for Qwen and Meta AI after manual authentication.

**Steps**

1. Use the dashboard or `./go-proxy --login <provider>` to authenticate manually.
2. Save the session.
3. Run `./go-proxy --test-adapter <provider> --config config.yaml`.
4. POST a short non-streaming chat request using the provider's model alias.
5. POST a short streaming request.
6. Reuse the returned `X-Conversation-Id` for a follow-up turn.
7. Inspect `/health` and the dashboard after the requests.

**Expected**

- The session becomes available after saving.
- The adapter check finds input and Send elements and observes a response token through
  its diagnostic mutation-observer path. This checks selectors but does not replace a
  production API response-polling test.
- Non-streaming output contains assistant text and `X-Provider`.
- Streaming output emits valid SSE chunks and terminates cleanly.
- The conversation ID reuses the same browser tab and preserves context.
- Health remains available and the dashboard shows request activity.
- The provider does not require a raw password after the session is saved.

### BT-08 — Routing and fallback

**Steps**

1. With at least two authenticated providers, send a request using `model: "auto"`.
2. Send a request using each configured alias.
3. Make one selected provider unavailable by removing only its session file or
   logging it out in the provider UI.
4. Repeat the `auto` request.

**Expected**

- `auto` starts with the lowest priority number among eligible providers.
- Explicit aliases select the expected provider.
- An unavailable provider is skipped or marked degraded and the next eligible
  provider can answer before response bytes are committed.
- The response identifies the provider used.
- The failed provider does not make the whole proxy unhealthy.

### BT-09 — Session expiry and recovery

**Steps**

1. Expire or invalidate one provider session using the provider's own logout
   control, or use a disposable beta account.
2. Send a request to that provider.
3. Confirm the dashboard reports the degraded/no-session state.
4. Re-authenticate manually and save a new session.
5. Repeat the request.

**Expected**

- The failed request returns a clear provider/API error or falls back when
  configured.
- The dashboard does not claim a healthy session indefinitely.
- Re-authentication restores the provider without restarting when supported.
- No password is written to logs or session files.

### BT-10 — Performance and response integrity

Run this case separately for Qwen and Meta AI with authenticated sessions.

**Steps**

1. Restart the proxy and record whether the first sample is a cold start. Issue a
   warm-up request only when intentionally measuring warm behavior.
2. Send at least three short exact-answer non-streaming requests to each provider.
3. Record HTTP status, `X-Provider`, fallback attempts, total duration, response content,
   and whether the answer matches exactly.
4. Repeat one request with `stream: true` and record time to first SSE chunk and total
   completion time.
5. Run one whitespace-sensitive coding response, compile it, and execute it in a
   constrained temporary directory.
6. Repeat a request with `X-Conversation-Id` to compare a retained tab with a new
   conversation.

**Expected**

- Every measured success is HTTP 200, identifies the requested provider, returns
  non-empty OpenAI-compatible output, and does not silently depend on fallback.
- Exact-answer probes are correct; code must compile and produce the expected output.
- Results report cold/warm or new/continued conversation state instead of combining
  unlike samples.
- Queue time, selector fallback, provider generation, and response completion are
  separated when stage telemetry is available.
- No fixed SLA is implied by a single run; report sample count, mean, minimum, maximum,
  failures, and environment.

The measurement method and current known results are maintained in Chapter 12.

## 11.5 Security checks

Run these checks in every beta cycle:

- Search logs and response bodies for `password`, `GMAIL_PASSWORD`,
  `GMAIL_APP_PASS`, cookie values, or localStorage values.
- Confirm session files and browser profiles are not publicly served.
- Confirm dashboard mutation routes are protected when
  `server.dashboard_secret` is configured.
- Confirm remote-login status, frame, input, save, and cancel routes return 401
  without the configured dashboard bearer token.
- Confirm stale login job identifiers cannot capture, control, save, or cancel a
  replacement browser.
- Confirm frame responses use `no-store` and exposed redirect labels contain no path,
  query string, fragment, OAuth code, or state value.
- Confirm no IMAP, magic-link, CAPTCHA relay, or automated provider-password
  path is reachable.
- Confirm the proxy is bound and firewalled as intended before any real account
  is used.
- Remove temporary fixtures and test artifacts after the run.

## 11.6 Current beta execution record

Recorded on 2026-08-29:

| Case | Result | Evidence / notes |
|---|---|---|
| BT-01 | Pass | `/health`, `/v1/models`, and provider API returned Qwen and Meta AI only |
| BT-02 | Pass | Fresh-browser dashboard check; supported provider cards visible, no CAPTCHA or auto-login UI |
| BT-03 | Pass | Qwen initial modal opened and closed; no password field |
| BT-04 | Pass | Meta AI rendered live provider pixels in the dashboard; click/scroll/Tab relay, safe-origin display, cancel, and immediate-close cleanup passed without credentials |
| BT-05 | Pass | Disposable empty Qwen session fixture changed state to `ready`, then back to `no_session` after removal |
| BT-06 | Pass | Missing messages and unknown model returned 400; valid streaming/non-streaming requests with no sessions returned 503; health remained available |
| BT-07 | Blocked | No authenticated provider session was available |
| BT-08 | Blocked | Requires at least two authenticated provider sessions |
| BT-09 | Not run | Requires a disposable authenticated account/session |
| BT-10 | Not run | The 2026-08-29 credential-free cycle had no authenticated provider sessions |

The browser beta tester observed one `404 Not Found` resource for the missing
favicon. It did not block the dashboard or login flow and is classified as a
minor polish issue.

### Post-beta performance experiment

On 2026-09-05, separate authenticated development probes measured Meta AI correction
and optimization and exercised whitespace-sensitive code output through both providers.
The work used the local headless Replit workflow and manually authenticated sessions.
It did not capture the exact checkpoint or Chromium version and did not complete every
BT-10 streaming and continuation step, so it is not recorded as a full BT-10 pass.
Results and limitations are documented in Chapter 12.

## 11.7 Exit criteria

The beta is ready for a wider group when:

- BT-01 through BT-06 pass completely.
- BT-07 passes for every provider intended for release.
- BT-08 passes with at least two providers and confirms fallback.
- BT-09 passes for one disposable account.
- BT-10 passes for every release provider, including output-integrity checks.
- No critical or high security issue remains open.
- All provider sessions used for testing are removed or retained only by their
  account owner in a private environment.
