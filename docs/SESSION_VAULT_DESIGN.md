# Encrypted provider session vault design

Status: design baseline; runtime vault is partially implemented and remains opt-in for issue #127. The default file store still writes reusable authentication material as plaintext in `sessions/<provider>.json`. With `CHAT_GATEWAY_SESSION_STORE=vault`, the runtime uses a versioned `CGSV1` AES-256-GCM envelope through `ring`, a fresh 12-byte nonce, provider-bound associated data, and a stable 32-byte key supplied as 64 hex characters through `CHAT_GATEWAY_VAULT_KEY`. Legacy JSON migration occurs on vault load. The browser-relay import path also requires vault mode. This implementation is not yet production-complete: OS-keychain integration, key rotation/list commands, comprehensive migration fault-injection tests, and full live-provider restoration verification remain open. Vault keys and legacy migration input buffers are zeroized on drop where practical; deserialized session objects still contain ordinary strings and need a broader secret-type redesign for comprehensive in-memory zeroization. Provider-scoped in-memory cookies are cleared on relay unlink, and relay imports attempt to hot-apply the new session. Keep the gateway loopback-only and protect the session directory.

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
