# Local native messaging development bridge (macOS)

Epic #1605. The local bridge is enabled by default from extension version 0.3.1 and does not change the loopback gateway, provider permissions, session vault or Obscura runtime. Disable the bridge or do not register its host on an untrusted user account. Chrome may require acceptance of its standard install/update permission warning. Local processes running as your macOS user can reach the private socket; this is not isolation from same-user malware. Only one Chrome profile can own the bridge at a time.

## Install

1. Load the repository's existing `extension/` directory at chrome://extensions (Developer mode).
2. Copy its exact 32-character extension ID (letters a–p).
3. From the repository root run `python3 scripts/native_reload_bridge.py install YOUR_EXTENSION_ID`. Requires Python 3.8+; install uses the absolute interpreter path. Do not use sudo.
4. Native messaging starts automatically; no Extension options click is required. If Chrome cannot discover the newly registered host, quit and reopen Chrome or allow the worker to retry on its one-minute reconnect alarm. An explicitly disabled bridge remains disabled until re-enabled from Extension options.
5. Run these commands:

```bash
python3 scripts/native_reload_bridge.py ping
python3 scripts/native_reload_bridge.py diagnostics
python3 scripts/native_reload_bridge.py reload_extension
python3 scripts/native_reload_bridge.py diagnostics
```

The CLI uses a private directory (~/.chat-gateway-native, mode 0700), a Unix domain socket (0600) and length-prefixed JSON. Native registration is under ~/Library/Application Support/Google/Chrome/NativeMessagingHosts. The installer copies the reviewed host script; rerun installation after changing it, then reconnect the host. A long home path may exceed the OS socket path limit.

## Contract

Version 1 commands: ping, diagnostics, reload_extension. No arbitrary shell, eval, provider navigation, cookie exports or provider login automation. Diagnostics project only fixed provider names, booleans, category enums and bounded ChatGPT cookie-context counts. Gateway status comes from the existing read-only relay diagnostics API; counts come from the authorized Chrome capture attempt. A saved session is not verified authentication or successful chat. No raw gateway response, account identifier, cookie, token or URL is returned.

Reload is refused while provider sync, consent mutation or scheduled refresh is active. A successful preparation is not proof of completion. Host forwards the accepted response to the CLI before sending a matching internal reload_commit. The worker records the request receipt before calling runtime.reload. CLI waits up to 75 seconds for a different worker instance with the same receipt. This provides correlated restart evidence, not proof that a new source version was installed; compare reported version and use actual Chrome acceptance to verify source changes. No automatic retry of the reload command. Failure may be indeterminate; inspect ping before manually retrying. Pending browser debounce timers may be lost on reload; existing startup and periodic resync remain responsible for restoration.

### Automatic provider session refresh after reload

Unpacked extension reload (manual or CLI) emits Chrome's update lifecycle event. The worker probes gateway status and force-syncs every provider that is both explicitly enabled in the gateway and already linked in the extension, even when its last cookie snapshot is unchanged. It does not grant permissions, link an account, navigate a provider tab, or overwrite a saved session with empty cookies. Disabled, unlinked and unknown-status providers are excluded from the forced batch. Regular startup and 15-minute heartbeats remain digest-based; explicitly disabled providers are skipped. An overlapping heartbeat is coalesced or followed by one forced batch. If the gateway is temporarily unavailable, the worker retains pending reload intent for the next heartbeat while that worker remains alive. A worker shutdown can lose that in-memory retry intent.

The CLI reload result confirms worker restart, not provider session upload completion. Run diagnostics to observe per-provider sync_state (e.g. syncing, uploaded, no_cookies, saved_activation_pending, failed) and gateway health; no automatic credential or permission escalation occurs. A saved session is not proof of successful chat or verified account authentication.

## Remove

Select **Disable local bridge** from Extension options first. This persists across reloads and closes the active port; it does not revoke Chrome's required nativeMessaging permission. Then run `python3 scripts/native_reload_bridge.py uninstall`. This removes only host registration; the private host directory remains for explicit cleanup after Chrome stops the host.

## Verification

```bash
node --test extension/tests/native_bridge.test.cjs
python3 -m unittest discover -s scripts -p test_native_reload_bridge.py
```

Existing Rust CI discovers both suites. Actual Chrome acceptance on the owner Mac is tracked by #1610. Validate ping, persistent local opt-out, missing host, restart, reload while busy, and provider diagnostics against ChatGPT and Meta AI. Provider account login and permissions remain explicit user actions.

### ChatGPT partition capture diagnostics

Extension 0.3.2 adds only bounded numeric `partition_counts` to the current
ChatGPT sync attempt. They distinguish unpartitioned cookies, the current
ChatGPT site with/without a cross-site ancestor, unspecified ancestor scope,
foreign-site partitions and unknown metadata. At most 512 unique cookie scopes
are counted; `limited` marks a truncated observation. These are metadata counts,
not authenticated-account evidence. Values, names, domains and partition URLs
stay inside Chrome. Counts are permission/consent scoped and rejected snapshots
still fail before upload; no partition scope is dropped or flattened.
