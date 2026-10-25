# Adapter canary diagnostics

The live adapter diagnostic checks session state, input/send selectors, a test
send, response detection, and model discovery. It interacts with provider sites
and may send a small diagnostic prompt, so run it only against accounts and
providers you are authorized to use. It requires the configured provider site to
be reachable and a valid session for a healthy result.

Run all enabled providers and emit a machine-readable summary:

```sh
cargo run --locked -- --config config.yaml --all --json
```

The command exits non-zero if any enabled provider fails the diagnostic or model
discovery. Run one provider with `--test-adapter qwen --json`. The human-readable
form remains available by omitting `--json`.

## Cron example

Use a local installation with persistent config/session storage. The following
runs every night at 03:20 and appends JSON output to a restricted local log:

```cron
20 3 * * * cd /opt/chat-gateway && umask 077 && /usr/bin/cargo run --locked -- --config /opt/chat-gateway/config.yaml --all --json >> /var/log/chat-gateway-adapter-canary.jsonl 2>&1
```

The process exit code is non-zero on failure. Have the local scheduler or
monitoring agent alert on a non-zero exit; do not publish session material or
unredacted diagnostic logs to a shared service. A live canary should be scheduled
only after provider sessions have been linked and verified locally.
