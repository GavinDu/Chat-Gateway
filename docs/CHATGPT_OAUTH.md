# ChatGPT account sign-in transport (opt-in)

ChatGPT browser automation can display a valid composer and still fail to submit a generation. This optional transport uses the supported **Sign in with ChatGPT for open-source and locally hosted apps** flow and the public Responses endpoint. It needs an explicit account authorization; Chrome cookies and a single earlier HTTP 200 do not establish that this transport works for an account.

Official protocol references:

- [Registration and sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)
- [Models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
- [Accounts, refresh and revocation](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions)
- [Preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)

## Connect

Use this only with an image built from the reviewed commit. Before release gates pass, set `CHAT_GATEWAY_IMAGE` to the commit-specific candidate image; do not overwrite the installed `chat-gateway-rs:local` tag. A feature candidate is for testing, not a validated installed release.

```sh
bash scripts/docker-run.sh chatgpt-connect default
bash scripts/docker-run.sh chatgpt-auth-status
```

The helper opens a local page with **Continue with ChatGPT**. Review the app and plan permissions in the system browser. Authentication, MFA and consent stay in that browser; no API key or client secret is required. Availability and actual plan permission must be confirmed with the selected account. Nothing reuses credentials belonging to Codex or another app.

If sign-in grants identity only, the gateway saves that registration without activating it for inference. To explicitly review plan permission again:

```sh
bash scripts/docker-run.sh chatgpt-enable-plan default
```

Set this provider in the effective gateway configuration:

```yaml
providers:
  - name: chatgpt
    enabled: true
    transport: chatgpt_oauth
    models: [chatgpt]
    timeout_seconds: 120
    # oauth_model: <visible slug from this account's model catalog>
```

Keep encrypted vault storage enabled. The default remains `transport: browser`. No automatic fallback to an API key or another provider is enabled by this change. The generic `chatgpt` alias uses the first visible account model unless `oauth_model` selects a visible slug explicitly. An unavailable explicit model is an error, not a silent substitution.

To use another account or workspace, run `chatgpt-connect` with a different stable profile label. The validated profile becomes active only after plan permission is granted. To return to an existing profile, connect that label again; its issued client ID and this host's ID are reused. Never copy the credential vault to a different host as a new installation.

## Session storage and disconnect

Credentials live in a separate encrypted `sessions/chatgpt-oauth.vault` record, authenticated with its own namespace. Owner-only files, atomic replacement and a cross-process lock protect rotating refresh tokens. Backup and key rotation include this record. Keep the existing vault-key volume; never print or attach credential files. Registration and verified account identity remain after disconnect so reauthorization cannot silently change the registered account.

```sh
bash scripts/docker-run.sh chatgpt-disconnect default
```

Disconnect attempts remote revocation and clears local tokens. If remote revocation is not confirmed, disconnect the app in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage). Plan limits and credit preferences are controlled there. Browser-cookie unlink is deliberately unavailable in OAuth mode; reload the updated extension to show gateway-managed account status and stop automatic cookie uploads.

## Completion and error contract

The upstream request always uses `store: false`, `stream: true` and the complete bounded, owner-scoped gateway transcript. Existing remote browser-thread URLs are not reused. This flow does not import ChatGPT website conversation history. Both gateway JSON and streaming outputs require `response.completed`; partial text, premature EOF, failed/incomplete events and timeouts never commit a conversation turn.

An upstream HTTP 401 can renew authorization once before accepted inference. Accepted or partially streamed generation is never automatically replayed. Concurrent refreshes are serialized. Transient refresh failures retain credentials; unusable refresh tokens require reconnecting with the retained registration. Policy denial is HTTP 403; usage limits are HTTP 429 without an invented reset time. A streaming error after headers is an error event, not a successful stop.

## Acceptance before calling this reliable

Fixtures verify protocol behavior, not a live ChatGPT account. Record the exact commit and account-specific results without secrets:

1. Complete sign-in and verify the account-specific model catalog.
2. Alternate at least five JSON and five streaming gateway requests with independent conversation IDs. Require nonempty replies; streaming must reach a normal stop with no error event.
3. Continue a conversation and verify earlier context is included once. Repeat with interleaved conversations to check isolation.
4. Restart the isolated candidate with the same disposable authorized state; repeat both response modes.
5. Verify renewal after real access-token expiry, and reconnect after explicit disconnect. Do not fabricate an expiry in production state.
6. Confirm quota/policy errors remain recognizable and failed/partial turns are not saved.
7. Pass current-commit CI and applicable MCP sandbox E2E, then the merged-main gates in [DEVELOPMENT_WORKFLOW.md](DEVELOPMENT_WORKFLOW.md), before installing a release.

Live authorization, expiry renewal and repeat inference are pending until exercised with a consenting account. No fixture result constitutes provider acceptance.
