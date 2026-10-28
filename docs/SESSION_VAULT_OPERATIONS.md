# Encrypted session vault operations

This guide applies when `CHAT_GATEWAY_SESSION_STORE=vault` is enabled. Vault files contain reusable provider credentials; treat both the files and the key as production secrets.

## Key provisioning

1. Generate a fresh 32-byte key using a cryptographically secure random generator. Encode it as 64 hexadecimal characters, as required by `CHAT_GATEWAY_VAULT_KEY`.
2. Store the key in a dedicated deployment secret manager (for example, your cloud secret manager or Kubernetes Secret backed by your organization's secret store). Inject it into the service environment at runtime. Do not commit it, place it in a container image, pass it as a command-line argument, print it, or write it to ordinary application logs.
3. Restrict who can read/update the secret and who can read the `sessions/` directory. Keep encrypted backups and the matching key under separate access controls.
4. Enable vault mode explicitly with `CHAT_GATEWAY_SESSION_STORE=vault`. Startup must fail closed when the key is absent or malformed; do not fall back to plaintext storage to recover from a decryption error.

The current application consumes the key from an environment variable; it does not retrieve keys directly from an OS keychain or cloud secret manager. The deployment platform is responsible for secret injection and rotation policy.

## Inventory and deletion

Run these commands with the same vault key and working directory used by the service:

```sh
# Show provider IDs and record status only; no cookie values are printed.
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-list

# Delete one provider's persisted session (vault and legacy snapshot).
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-delete-provider chatgpt
```

Deletion also clears the persisted legacy snapshot, but use the gateway's authenticated unlink flow when you need the running embedded browser's in-memory cookies and origin storage cleared as well.

## Key rotation and recovery

Rotation uses `CHAT_GATEWAY_VAULT_KEY` as the old key and `CHAT_GATEWAY_VAULT_NEW_KEY` as the replacement key. Both must be provided by the deployment secret manager for the rotation process.

```sh
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-rotate-key
```

The command preflights encrypted records, verifies replacements, and retains encrypted old records under `sessions/.vault-rotation-*`. Multi-file replacement is **recoverable, not crash-atomic**: preserve the printed backup directory and old key until the service has restarted and every provider record has been checked. Do not delete backups as part of the same automated step as rotation.

If rotation is interrupted and the current vault records need to be restored, inject the **old** key as `CHAT_GATEWAY_VAULT_KEY` and run:

```sh
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-restore-backup sessions/.vault-rotation-<id>
```

Then restart the gateway with the restored key, inspect the vault inventory, and verify providers one by one. Restore only from a backup directory produced by the rotation command; do not manually edit encrypted records.

## Migration behavior

On first load in vault mode, a legacy `sessions/<provider>.json` file is migrated only after an encrypted replacement has been written and authenticated by read-back. If migration fails, keep the original file and investigate the filesystem permissions, free space, and key configuration. Do not manually delete the legacy source to silence an error.

## Operational checklist

- [ ] Secret manager retains the active key and a separately controlled recovery copy.
- [ ] Key injection is configured for service startup and one-off maintenance commands.
- [ ] Backups are encrypted, access-controlled, and stored separately from the active key.
- [ ] After rotation, restart the gateway and verify the inventory and live provider sessions before removing old-key recovery material.
- [ ] Never attach session snapshots, keys, or unredacted provider logs to tickets.
