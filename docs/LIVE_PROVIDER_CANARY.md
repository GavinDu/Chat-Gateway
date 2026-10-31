# Live Obscura-backed provider canary

This is an opt-in, real-account check. It is not part of ordinary CI and must
only be run against provider accounts and sessions you are authorized to use.
It sends short synthetic prompts through the OpenAI-compatible endpoint.

## Configure

Set the gateway endpoint, an API key accepted by the gateway, and the exact
model IDs exposed by your local `/v1/models` endpoint. Model IDs should include
the provider prefix used by routing (for example, `chatgpt/<configured-id>`).

```sh
export CHAT_GATEWAY_URL=http://127.0.0.1:8080
export CHAT_GATEWAY_API_KEY='...'
export CANARY_CHATGPT_MODEL='chatgpt/<model-id>'
export CANARY_CLAUDE_MODEL='claude/<model-id>'
export CANARY_GEMINI_MODEL='gemini/<model-id>'
export CANARY_GROK_MODEL='grok/<model-id>'
# Optional:
export CHAT_GATEWAY_CANARY_TIMEOUT=180
export CHAT_GATEWAY_CANARY_STATE=/tmp/chat-gateway-live-canary-state.json
```

Do not commit these values or put the API key in command-line arguments.

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
   conversation ID. A missing marker, missing SSE `[DONE]`, empty response,
   provider error, HTTP error or timeout fails the run. The local state file is
   deleted after a fully successful second phase.

## Interpreting the report

- `ttft_ms` is measured to the first non-empty content delta.
- `total_ms` includes the complete streamed turn through terminal `[DONE]`.
- `failure_category` is intentionally sanitized; provider response bodies,
  prompts, cookies, authorization headers and response text are not emitted.
- A passing report is evidence for that specific run/account/session, not a
  blanket guarantee that a provider is reliable.

The gateway process restart is intentionally manual: the script must not
terminate or restart a process it does not own. Run the second phase only after
the restart has completed.
