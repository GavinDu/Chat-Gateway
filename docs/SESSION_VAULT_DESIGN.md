# Encrypted provider session vault design

Status: the opt-in runtime vault is implemented, but issue #127 remains open for remaining hardening and rollout work. The default file store still writes reusable authentication material as plaintext in `sessions/<provider>.json`; explicitly set `CHAT_GATEWAY_SESSION_STORE=vault` to use the encrypted store. Vault mode uses a versioned `CGSV1` AES-256-GCM envelope through `ring`, a fresh 12-byte nonce, provider-bound associated data, and a stable 32-byte key supplied as 64 hex characters through `CHAT_GATEWAY_VAULT_KEY`. Runtime code includes legacy JSON migration with read-back verification, key rotation/list/restore maintenance commands, zeroizing key/plaintext buffers where available, owner-only Unix permissions, and verified in-memory cookie clearing during relay unlink. The browser-relay import path also requires vault mode. Remaining work includes OS-keychain integration, broader end-to-end restart/rotation fault-injection tests, and deciding whether plaintext mode should ever cease to be the default. Keep the gateway loopback-only and protect the session directory.

## Security goals

- Protect session contents at rest when a session file or backup is copied without the vault key.
- Authenticate ciphertext and its metadata so a modified or swapped provider record fails closed.
- Avoid logging cookies, localStorage values, plaintext session JSON, keys, or decrypted payloads.
- Preserve an explicit migration path and never silently discard sessions on decryption or migration failure.

## Threat boundary

Encryption at rest helps against disclosure of copied files and backups when the key is stored separately. It does not protect a running gateway from a local process with the same user privileges, memory inspection, a compromised host, or an attacker who can read both the ciphertext and key. The login relay and provider session data remain highly sensitive; remote exposure is unsupported.

## Proposed envelope

Use an authenticated-encryption implementation from a maintained Rust cryptography crate, with a versioned envelope such as:

- `format_version`: integer, initially `1`
- `algorithm`: fixed identifier for the selected AEAD
- `nonce`: fresh cryptographically random nonce for every encryption
- `ciphertext`: authenticated ciphertext of the existing serialized session payload

The associated data should bind a domain separator, provider name, and format version. The provider name in the path and envelope must agree. Reject unknown versions, algorithms, malformed lengths, authentication failures, and provider mismatches. Never fall back to parsing ciphertext as plaintext.

The implementation should use a well-reviewed AEAD such as XChaCha20-Poly1305, with a fresh 24-byte nonce generated from the operating system CSPRNG. Do not implement cryptographic primitives directly. Pin the dependency in `Cargo.lock` and document the update/audit procedure.

## Key management

Preferred order:

1. Use an OS credential store/keychain through a supported, reviewed integration where available.
2. For headless deployments, allow an explicitly configured 32-byte key supplied by the process environment or a protected secret file. The exact interface should be selected during implementation and documented for Docker.
3. Never generate an ephemeral key on each startup: that would make existing sessions undecryptable after restart.
4. Never store the encryption key beside the encrypted session files, in the repository, in logs, or in a container image.

Key absence, invalid key length, or failed authentication must produce an actionable error and leave the original file untouched. Do not silently disable encryption when vault mode has been configured.

## Operator commands and key rotation

The binary provides explicit maintenance commands that exit before starting the browser runtime:

- `chat-gateway-rs --vault-list`: prints one row per supported provider with `valid`, `missing`, `unreadable`, or `legacy-plaintext`. It requires the current `CHAT_GATEWAY_VAULT_KEY`; it never prints cookie or localStorage values.
- `chat-gateway-rs --vault-delete-provider chatgpt`: deletes that provider's encrypted snapshot and any legacy plaintext snapshot. Treat this as destructive credential revocation.
- `CHAT_GATEWAY_VAULT_NEW_KEY` plus `chat-gateway-rs --vault-rotate-key`: decrypts every present encrypted record with `CHAT_GATEWAY_VAULT_KEY`, validates all replacements with the new key, writes encrypted recovery copies under `sessions/.vault-rotation-*`, then replaces the live records. Runtime write errors trigger rollback of files already replaced.
- `chat-gateway-rs --vault-restore-backup sessions/.vault-rotation-...`: restores a rotation backup. Set `CHAT_GATEWAY_VAULT_KEY` to the old key before running this command. The command validates every backup record before writing any of them.

After a successful rotation, update the deployment's `CHAT_GATEWAY_VAULT_KEY` to the new value before restarting the gateway. Keep the old key and encrypted recovery directory until `--vault-list` reports expected records as valid and restart restoration has been verified. Store keys in a deployment secret manager or OS-backed secret store; do not put them in command-line arguments, source control, logs, or container images. The environment-variable interface is an operator-managed key-injection mechanism, not OS-keychain integration.

The recovery directory makes interrupted rotations recoverable, but the multi-file replacement is not crash-atomic. If the process or host stops mid-rotation, restore the complete backup using the old key, then retry rotation. Do not switch the configured key until rotation has completed.

## Migration and recovery

- Detect the legacy JSON format separately from the encrypted envelope.
- In an explicit migration mode, parse and validate a legacy record, encrypt it, write a temporary file with owner-only permissions, fsync it, atomically replace the target, and retain a recoverable backup until validation succeeds.
- Do not delete or overwrite the only copy if encryption, write, fsync, rename, or read-back validation fails.
- Make migration idempotent. An already-encrypted file must not be re-encrypted as if it were legacy JSON.
- Document key backup and rotation before enabling vault mode by default. Rotation must decrypt with the old key and re-encrypt with the new key atomically; losing the only key is expected to make sessions unrecoverable.

## Required implementation tests

- Round-trip serialization for each provider session shape.
- Wrong key, tampered ciphertext, invalid nonce, unknown version, wrong provider, and truncated envelope all fail closed.
- Legacy migration succeeds, preserves the original session semantics, and is idempotent.
- Simulated write/rename failure leaves the legacy source recoverable.
- Session files and temporary files have owner-only permissions on Unix.
- Error messages and tracing output contain no cookie values, localStorage values, plaintext payloads, or key material.
- Restart with the same configured key restores sessions; restart without the key fails closed.
- Existing login, cookie-domain validation, localStorage restoration, and provider warm-up tests remain green.

## Rollout

Ship the encrypted reader/writer behind an explicit vault configuration first. Require a successful round-trip and migration test suite before recommending migration. Keep plaintext legacy import opt-in and emit a warning that identifies the provider/file path only, never the contents. A later change can decide whether encryption becomes mandatory for all persistent sessions.


## Vault management commands

Vault administration is explicit and runs before browser startup:

- `CHAT_GATEWAY_SESSION_STORE=vault CHAT_GATEWAY_VAULT_KEY=<64 hex chars> cargo run -- --vault-list` lists each supported provider as `valid`, `missing`, or `unreadable`. It never prints session values.
- `cargo run -- --vault-delete-provider chatgpt` removes the named provider's persisted vault and legacy snapshot. This does not itself clear a running browser's in-memory cookie jar; use the authenticated relay unlink endpoint for live revocation.
- To rotate a key, set `CHAT_GATEWAY_VAULT_KEY` to the current key and `CHAT_GATEWAY_VAULT_NEW_KEY` to a different 64-character hex key, then run `cargo run -- --vault-rotate-key`. Rotation validates every existing vault record before replacing any, verifies replacement ciphertext, and retains encrypted recovery copies under `sessions/.vault-rotation-<id>/`. After success, update `CHAT_GATEWAY_VAULT_KEY` to the new key and verify restart restoration before removing backups or retiring the old key.

The environment-key mechanism is a portable fallback, not an OS-keychain integration. In production, inject the key from a secret manager or OS-protected store; never commit it, print it, or place it in logs. Keep recovery copies owner-only and encrypted. A missing/wrong key must fail closed; do not fall back to plaintext when an encrypted record exists.
