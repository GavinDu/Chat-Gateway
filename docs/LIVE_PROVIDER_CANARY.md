# Live Obscura-backed provider canary

This is an opt-in, real-account check. It is not part of ordinary CI and must
only be run against provider accounts and sessions you are authorized to use.
It sends short synthetic prompts through the OpenAI-compatible endpoint.

## Configure

Run this against a clean **main-branch** installation. Set the gateway
endpoint and the exact model IDs exposed by its `/v1/models` endpoint, including
each model's `owned_by` provider. No provider API keys are needed: the canary
uses browser-linked provider sessions. A gateway API key is different from a
provider API key and is needed only when your gateway API authentication is enabled.

```sh
export CHAT_GATEWAY_URL=http://127.0.0.1:8080
export CHAT_GATEWAY_API_KEY='...'  # omit if local gateway API auth is disabled
export CANARY_CHATGPT_MODEL='chatgpt/<model-id>'
export CANARY_CLAUDE_MODEL='claude/<model-id>'
export CANARY_GEMINI_MODEL='gemini/<model-id>'
export CANARY_GROK_MODEL='grok/<model-id>'
# Optional:
export CHAT_GATEWAY_CANARY_TIMEOUT=180
export CHAT_GATEWAY_CANARY_STATE=/tmp/chat-gateway-live-canary-state.json
```

Do not commit these values or put the API key in command-line arguments.

If the gateway has **no API key configured** and listens on loopback, you may
explicitly opt into keyless local canary requests:

```sh
unset CHAT_GATEWAY_API_KEY
export CHAT_GATEWAY_LOCAL_CANARY_NO_API_KEY=1
```

This override is valid only for loopback HTTP. It is rejected for HTTPS/remote
origins, and an API-authenticated gateway will still reject unauthenticated
requests. Never disable gateway authentication on a public deployment.

First perform a **read-only preflight**. It checks gateway health and that all
four configured model IDs appear under the correct provider in `/v1/models`,
without submitting any provider prompts or saving credentials:

```sh
python3 scripts/live_provider_canary.py --phase preflight
```

A preflight failure prints a sanitized error category; fix the missing model,
authorization or gateway health before running the live phases. The preflight
does **not** prove that browser sessions are authenticated; check the extension's
provider status and the real live run for that. Every phase repeats preflight
before sending a synthetic marker.

## Run the restart and provider-switch sequence

1. Ensure ChatGPT, Claude, Gemini and Grok are linked and healthy in the local
   Obscura session store. Keep the gateway's SQLite database and browser session
   storage on persistent volumes.
2. Run the first two ChatGPT turns:

   ```sh
   python3 scripts/live_provider_canary.py --phase before-restart
   ```

   The script creates a random synthetic continuity marker, checks a ChatGPT
   follow-up on the same gateway conversation, and stores only the conversation
   ID and marker in a mode-0600 local state file. Its JSON report contains
   provider/scenario, status, TTFT and total duration, not prompts or answers.
3. Stop and restart Chat-Gateway normally. Do not clear SQLite, Obscura
   `storage_dir`, or provider session snapshots. Confirm the gateway is ready.
4. Run the provider-switch checks:

   ```sh
   python3 scripts/live_provider_canary.py --phase after-restart
   ```

   Claude, Gemini and Grok must each recover the marker through the same
   conversation ID. A missing marker, missing SSE `[DONE]`, missing successful terminal
   `finish_reason: stop`, empty response,
   provider error, HTTP error or timeout fails the run. The local state file is
   deleted after a fully successful second phase.

## Interpreting the report

- `ttft_ms` is measured to the first non-empty content delta.
- `total_ms` includes the complete streamed turn through successful
  `finish_reason: stop` and terminal `[DONE]`.
- `failure_category` is intentionally sanitized; provider response bodies,
  prompts, cookies, authorization headers and response text are not emitted.
- A passing report is evidence for that specific run/account/session, not a
  blanket guarantee that a provider is reliable.

The gateway process restart is intentionally manual: the script must not
terminate or restart a process it does not own. Run the second phase only after
the restart has completed.
