# Encrypted session vault operations

This guide applies when `CHAT_GATEWAY_SESSION_STORE=vault` is enabled. Vault files contain reusable provider credentials; treat both the files and the key as production secrets.

## Key provisioning

1. Generate a fresh 32-byte key using a cryptographically secure random generator. Encode it as exactly 64 hexadecimal characters.
2. Supply the key through exactly one of these supported mechanisms:
   - `CHAT_GATEWAY_VAULT_KEY`: inject the value through the deployment's secret manager at runtime.
   - `CHAT_GATEWAY_VAULT_KEY_FILE`: point to a regular, non-symlink file containing the 64-character hexadecimal key. On Unix, group and other permission bits must be clear (for example, mode `0600`). Mount the file read-only where possible.
3. Keep the key outside the repository, ordinary configuration files, logs, container image, and extension storage. Never pass it as a command-line argument or print it. Restrict who can read or replace it.
4. Enable vault mode explicitly with `CHAT_GATEWAY_SESSION_STORE=vault`. Startup fails closed when the key is absent or malformed; the application does not fall back to plaintext storage to recover from a decryption error.
5. Keep encrypted backups and the matching key under separate access controls. Losing the active key without a matching recovery key makes the vault records unreadable.

The application reads the key from the runtime environment or from the protected key file named by `CHAT_GATEWAY_VAULT_KEY_FILE`; it does not retrieve keys directly from an OS keychain or cloud secret manager. The deployment platform is responsible for secret injection, file mounting, access policy, and rotation.

## Inventory and deletion

Run these commands with the same vault key and working directory used by the service:

```sh
# Show provider IDs and record status only; no cookie values are printed.
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-list

# Delete one provider's persisted session (vault and legacy snapshot).
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-delete-provider chatgpt
```

The CLI reads the key using the same environment/key-file mechanism as the service. Deletion also clears the persisted legacy snapshot, but use the gateway's authenticated unlink flow when you need the running embedded browser's in-memory cookies and origin storage cleared as well.

## Key rotation and recovery

Rotation uses `CHAT_GATEWAY_VAULT_KEY` (or `CHAT_GATEWAY_VAULT_KEY_FILE` for the old key) as the current key and `CHAT_GATEWAY_VAULT_NEW_KEY` as the replacement key. The replacement key is supplied through the runtime environment for the one-off rotation process. Keep both keys in the deployment secret manager during the operation; never expose their values in logs or command-line arguments.

```sh
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-rotate-key
```

The command preflights encrypted records, verifies replacements, and retains encrypted old records under `sessions/.vault-rotation-*`. Multi-file replacement is **recoverable, not crash-atomic**: preserve the printed backup directory and old key until the service has restarted and every provider record has been checked. Do not delete backups as part of the same automated step as rotation.

If rotation is interrupted and the current vault records need to be restored, supply the **old** key through `CHAT_GATEWAY_VAULT_KEY` or `CHAT_GATEWAY_VAULT_KEY_FILE` and run:

```sh
CHAT_GATEWAY_SESSION_STORE=vault cargo run -- --config config.yaml --vault-restore-backup sessions/.vault-rotation-<id>
```

Then restart the gateway with the restored key, inspect the vault inventory, and verify providers one by one. Restore only from a backup directory produced by the rotation command; do not manually edit encrypted records.

## Migration behavior

On first load in vault mode, a legacy `sessions/<provider>.json` file is migrated only after an encrypted replacement has been written and authenticated by read-back. If migration fails, keep the original file and investigate filesystem permissions, free space, and key configuration. Do not manually delete the legacy source to silence an error.

## Operational checklist

- [ ] Secret manager retains the active key and a separately controlled recovery copy.
- [ ] Key injection or protected key-file mounting is configured for service startup and one-off maintenance commands.
- [ ] Backups are encrypted, access-controlled, and stored separately from the active key.
- [ ] After rotation, restart the gateway and verify the inventory and live provider sessions before removing old-key recovery material.
- [ ] Never attach session snapshots, keys, or unredacted provider logs to tickets.
