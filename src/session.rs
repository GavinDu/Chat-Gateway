use anyhow::Result;
use obscura_core::{Browser, Page};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Serialize, Deserialize)]
pub struct SessionFile {
    pub local_storage: std::collections::HashMap<String, String>,
    pub cookies: Vec<Cookie>,
    pub ts: String,
}

impl Drop for SessionFile {
    fn drop(&mut self) {
        for value in self.local_storage.values_mut() {
            value.zeroize();
        }
        // Cookie::drop zeroizes each reusable credential value.
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
}

impl Drop for Cookie {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl From<obscura_core::Cookie> for Cookie {
    fn from(c: obscura_core::Cookie) -> Self {
        Self {
            name: c.name,
            value: c.value,
            domain: c.domain,
            path: c.path,
            secure: c.secure,
            http_only: c.http_only,
        }
    }
}

pub fn session_path(provider: &str) -> Result<String> {
    // Provider IDs become filesystem path components. Keep the accepted set
    // aligned with the provider registry; never interpolate arbitrary config
    // or request strings into a path.
    const PROVIDERS: &[&str] = &[
        "qwen",
        "metaai",
        "claude",
        "chatgpt",
        "gemini",
        "grok",
        "mistral",
        "perplexity",
    ];
    if !PROVIDERS.contains(&provider) {
        anyhow::bail!("unsupported provider identifier for session storage");
    }
    Ok(format!("sessions/{}.json", provider))
}

#[cfg(test)]
mod session_path_tests {
    use super::*;

    #[test]
    fn only_registered_provider_names_become_paths() {
        assert_eq!(session_path("chatgpt").unwrap(), "sessions/chatgpt.json");
        assert_eq!(
            session_path("perplexity").unwrap(),
            "sessions/perplexity.json"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_directory_is_hardened_to_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "chat-gateway-session-permissions-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_session_dir(&path).await.unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn rejects_path_traversal_and_unregistered_names() {
        assert!(session_path("../outside").is_err());
        assert!(session_path("chatgpt/../../outside").is_err());
        assert!(session_path("CHATGPT").is_err());
        assert!(session_path("").is_err());
    }
}

/// Storage abstraction for provider authentication state. The file store is
/// still the default; the encrypted vault can implement this seam without
/// changing browser warm-up or login call sites.
#[async_trait::async_trait(?Send)]
pub trait SessionStore {
    fn load(&self, provider: &str) -> Result<Option<SessionFile>>;
    async fn save_if_changed(&self, browser: &Browser, page: &Page, provider: &str) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileSessionStore;

#[async_trait::async_trait(?Send)]
impl SessionStore for FileSessionStore {
    fn load(&self, provider: &str) -> Result<Option<SessionFile>> {
        let path = session_path(provider)?;
        load_session_file_from_path(Path::new(&path))
    }

    async fn save_if_changed(&self, browser: &Browser, page: &Page, provider: &str) -> Result<()> {
        save_session_if_changed_file(browser, page, provider).await
    }
}

/// Encrypted on-disk session storage. Select it at runtime with
/// CHAT_GATEWAY_SESSION_STORE=vault and provide a 32-byte hex key through
/// CHAT_GATEWAY_VAULT_KEY. Missing or invalid keys fail closed.
#[derive(Debug, Clone, Copy, Default)]
pub struct VaultSessionStore;

#[async_trait::async_trait(?Send)]
impl SessionStore for VaultSessionStore {
    fn load(&self, provider: &str) -> Result<Option<SessionFile>> {
        let key = vault_key_from_env()?;
        let vault_path = session_vault_path(provider)?;
        match std::fs::read(&vault_path) {
            Ok(bytes) => {
                let session = decrypt_session_snapshot(&bytes, provider, &key)?;
                remove_legacy_session_snapshot(provider)?;
                return Ok(Some(session));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        // One-time migration: only remove the legacy plaintext snapshot after
        // the encrypted replacement has been durably written.
        let legacy_path = session_path(provider)?;
        let legacy = match std::fs::read(&legacy_path) {
            Ok(bytes) => serde_json::from_slice::<SessionFile>(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure_private_session_dir_sync(Path::new("sessions"))?;
        let encrypted = encrypt_session_snapshot(&legacy, provider, &key)?;
        write_session_file_atomically(Path::new(&vault_path), &encrypted)?;
        let verified = (|| -> Result<SessionFile> {
            let written = std::fs::read(&vault_path)?;
            let restored = decrypt_session_snapshot(&written, provider, &key)?;
            if session_state_hash(&restored)? != session_state_hash(&legacy)? {
                anyhow::bail!("session-vault read-back validation failed");
            }
            Ok(restored)
        })();
        let verified = match verified {
            Ok(session) => session,
            Err(error) => {
                let _ = std::fs::remove_file(&vault_path);
                return Err(error);
            }
        };
        // Keep the legacy source intact until the encrypted record has been read
        // back and authenticated successfully.
        std::fs::remove_file(&legacy_path)?;
        record_session_write(provider);
        tracing::info!(provider, "migrated plaintext session into encrypted vault");
        Ok(Some(verified))
    }

    async fn save_if_changed(&self, browser: &Browser, page: &Page, provider: &str) -> Result<()> {
        let key = vault_key_from_env()?;
        let file = capture_session(browser, page, provider).await?;
        let current_hash = session_state_hash(&file)?;
        let destination = session_vault_path(provider)?;
        let previous_hash = match std::fs::read(&destination) {
            Ok(bytes) => Some(session_state_hash(&decrypt_session_snapshot(
                &bytes, provider, &key,
            )?)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        if previous_hash.as_deref() == Some(current_hash.as_str()) {
            remove_legacy_session_snapshot(provider)?;
            return Ok(());
        }
        if session_write_is_rate_limited(provider, Instant::now()) {
            tracing::debug!(
                provider,
                "encrypted session changed but snapshot write is rate-limited"
            );
            return Ok(());
        }
        ensure_private_session_dir(Path::new("sessions")).await?;
        let encrypted = encrypt_session_snapshot(&file, provider, &key)?;
        write_session_file_atomically(Path::new(&destination), &encrypted)?;
        // Do not leave a plaintext copy behind once encrypted persistence works.
        remove_legacy_session_snapshot(provider)?;
        record_session_write(provider);
        tracing::info!(
            provider,
            cookies = file.cookies.len(),
            "encrypted session saved"
        );
        Ok(())
    }
}

/// Import a browser-linked session directly into the encrypted vault. Arbitrary
/// localStorage export is deliberately disabled until each provider has a
/// reviewed key allowlist.
pub fn import_relay_session(provider: &str, file: &SessionFile) -> Result<()> {
    if !matches!(
        std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref(),
        Ok("vault")
    ) {
        anyhow::bail!("browser relay requires CHAT_GATEWAY_SESSION_STORE=vault");
    }
    if !file.local_storage.is_empty() {
        anyhow::bail!(
            "localStorage import is disabled until provider-specific key allowlists are configured"
        );
    }
    if file.cookies.len() > 512 {
        anyhow::bail!("too many cookies in relay session");
    }
    for cookie in &file.cookies {
        if !cookie_domain_allowed(provider, &cookie.domain) || !restored_cookie_fields_safe(cookie)
        {
            anyhow::bail!("relay session contains a cookie outside the provider allowlist or with invalid fields");
        }
    }
    let key = vault_key_from_env()?;
    let destination = session_vault_path(provider)?;
    ensure_private_session_dir_sync(Path::new("sessions"))?;
    let encrypted = encrypt_session_snapshot(file, provider, &key)?;
    write_session_file_atomically(Path::new(&destination), &encrypted)?;
    let verified = (|| -> Result<()> {
        let written = std::fs::read(&destination)?;
        let restored = decrypt_session_snapshot(&written, provider, &key)?;
        if session_state_hash(&restored)? != session_state_hash(file)? {
            anyhow::bail!("session-vault read-back validation failed");
        }
        Ok(())
    })();
    if let Err(error) = verified {
        let _ = std::fs::remove_file(&destination);
        return Err(error);
    }
    remove_legacy_session_snapshot(provider)?;
    record_session_write(provider);
    tracing::info!(
        provider,
        cookies = file.cookies.len(),
        "browser relay session imported into encrypted vault"
    );
    Ok(())
}

fn cookie_expiration_header(cookie: &Cookie) -> Result<String> {
    if !restored_cookie_fields_safe(cookie) {
        anyhow::bail!("provider cookie has invalid fields and could not be safely revoked");
    }
    let path = if cookie.path.is_empty() {
        "/"
    } else {
        cookie.path.as_str()
    };
    let host_prefix = cookie.name.starts_with("__Host-");
    if (host_prefix && (path != "/" || !cookie.secure))
        || (cookie.name.starts_with("__Secure-") && !cookie.secure)
    {
        anyhow::bail!("provider cookie prefix constraints prevent safe revocation");
    }
    let mut expired = format!("{}=; Max-Age=0; Path={}", cookie.name, path);
    if host_prefix {
        expired.push_str("; Secure");
    } else {
        expired.push_str(&format!("; Domain={}", cookie.domain));
        if cookie.secure {
            expired.push_str("; Secure");
        }
    }
    if cookie.http_only {
        expired.push_str("; HttpOnly");
    }
    Ok(expired)
}

/// Remove cookies for one provider from Obscura's shared browser jar.
/// Every deletion is verified; failure aborts unlink before persistent records
/// are deleted, preventing a false success response for live credentials.
pub fn clear_provider_cookies(browser: &Browser, provider: &str) -> Result<usize> {
    let store = browser.cookies();
    let cookies = store.get_all();
    let mut cleared = 0;
    for browser_cookie in cookies {
        let mut cookie = Cookie::from(browser_cookie);
        // The old value is never written back; ignore it when validating the
        // deletion tuple so unusual but valid values do not leave stale cookies.
        cookie.value.clear();
        if !cookie_domain_allowed(provider, &cookie.domain) {
            continue;
        }
        let expired = cookie_expiration_header(&cookie)?;
        let path = if cookie.path.is_empty() {
            "/"
        } else {
            cookie.path.as_str()
        };
        let url = format!(
            "https://{}{}",
            cookie.domain.trim_start_matches('.'),
            if path.starts_with('/') { path } else { "/" }
        );
        store.set(&expired, &url)?;
        if store.get_all().iter().any(|remaining| {
            remaining.name == cookie.name
                && remaining.domain.trim_start_matches('.') == cookie.domain.trim_start_matches('.')
                && remaining.path == cookie.path
        }) {
            anyhow::bail!("provider cookie remained in the browser jar after revocation");
        }
        cleared += 1;
    }
    Ok(cleared)
}

pub fn delete_session_snapshot(provider: &str) -> Result<bool> {
    let vault_path = session_vault_path(provider)?;
    let legacy_path = session_path(provider)?;
    let mut removed = false;
    // Remove plaintext first. If this fails, retain the encrypted vault rather than
    // deleting the only usable copy and leaving a plaintext record behind.
    for path in [legacy_path, vault_path] {
        match std::fs::remove_file(path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(removed)
}

/// Non-secret status returned by the explicit vault inventory command.
#[derive(Debug, Clone)]
pub struct VaultSessionInfo {
    pub provider: String,
    pub status: &'static str,
}

/// List configured provider vault records without exposing cookie or storage values.
/// A malformed or wrong-key record is reported as "unreadable", never as plaintext.
pub fn list_vault_sessions() -> Result<Vec<VaultSessionInfo>> {
    let key = vault_key_from_env()?;
    const PROVIDERS: &[&str] = &[
        "qwen",
        "metaai",
        "claude",
        "chatgpt",
        "gemini",
        "grok",
        "mistral",
        "perplexity",
    ];
    let mut entries = Vec::with_capacity(PROVIDERS.len());
    for provider in PROVIDERS {
        let path = session_vault_path(provider)?;
        let status = match std::fs::read(&path) {
            Ok(bytes) => {
                if decrypt_session_snapshot(&bytes, provider, &key).is_ok() {
                    "valid"
                } else {
                    "unreadable"
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::metadata(session_path(provider)?) {
                    Ok(_) => "legacy-plaintext",
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => "missing",
                    Err(other) => return Err(other.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        entries.push(VaultSessionInfo {
            provider: (*provider).to_string(),
            status,
        });
    }
    Ok(entries)
}

/// Restore an encrypted pre-rotation backup. Configure the old key in
/// CHAT_GATEWAY_VAULT_KEY, then pass the printed backup directory.
pub fn restore_vault_backup(backup_dir: &str) -> Result<usize> {
    let path = Path::new(backup_dir);
    if !path.is_dir()
        || !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".vault-rotation-"))
        || path.parent() != Some(Path::new("sessions"))
    {
        anyhow::bail!("backup directory must be a sessions/.vault-rotation-* directory");
    }
    let key = vault_key_from_env()?;
    const PROVIDERS: &[&str] = &[
        "qwen",
        "metaai",
        "claude",
        "chatgpt",
        "gemini",
        "grok",
        "mistral",
        "perplexity",
    ];
    let mut backups = Vec::new();
    for provider in PROVIDERS {
        let backup_path = path.join(format!("{provider}.vault"));
        let bytes = match std::fs::read(&backup_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        decrypt_session_snapshot(&bytes, provider, &key).map_err(|_| {
            anyhow::anyhow!("backup record for {provider} is not valid with the configured key")
        })?;
        backups.push((session_vault_path(provider)?, bytes));
    }
    if backups.is_empty() {
        anyhow::bail!("no valid provider vault backups found in the specified directory");
    }
    for (destination, bytes) in &backups {
        write_session_file_atomically(Path::new(destination), bytes)?;
    }
    Ok(backups.len())
}

/// Rotate all encrypted vault records as one best-effort transaction.
/// Every input is decrypted and every replacement authenticated before any
/// live record is replaced. Encrypted old records are retained in a private
/// backup directory so an interrupted deployment can be recovered with the
/// old key. Runtime write failures trigger rollback of already-replaced files.
pub fn rotate_vault_key(new_key: &[u8; 32]) -> Result<(usize, String)> {
    let old_key = vault_key_from_env()?;
    if old_key[..] == new_key[..] {
        anyhow::bail!("new vault key must differ from the current key");
    }

    const PROVIDERS: &[&str] = &[
        "qwen",
        "metaai",
        "claude",
        "chatgpt",
        "gemini",
        "grok",
        "mistral",
        "perplexity",
    ];
    // Migrate any remaining legacy plaintext snapshots before rotation. The
    // vault loader verifies the encrypted replacement before removing plaintext.
    for provider in PROVIDERS {
        let _ = VaultSessionStore.load(provider)?;
    }
    let mut originals: Vec<(String, Vec<u8>, Vec<u8>)> = Vec::new();
    for provider in PROVIDERS {
        let path = session_vault_path(provider)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let session = decrypt_session_snapshot(&bytes, provider, &old_key).map_err(|_| {
            anyhow::anyhow!(
                "cannot rotate vault: {provider} record is unreadable with the current key"
            )
        })?;
        let replacement = encrypt_session_snapshot(&session, provider, new_key)?;
        let verified = decrypt_session_snapshot(&replacement, provider, new_key)?;
        if session_state_hash(&verified)? != session_state_hash(&session)? {
            anyhow::bail!("vault rotation verification failed for provider {provider}");
        }
        originals.push((path, bytes, replacement));
    }

    if originals.is_empty() {
        return Ok((0, String::new()));
    }

    let backup_dir = format!("sessions/.vault-rotation-{}", uuid::Uuid::new_v4());
    ensure_private_session_dir_sync(Path::new(&backup_dir))?;
    for (path, original, _) in &originals {
        let name = Path::new(path)
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("invalid vault path during key rotation"))?;
        write_session_file_atomically(&Path::new(&backup_dir).join(name), original)?;
    }

    let mut replaced = 0usize;
    for (path, _, replacement) in &originals {
        if let Err(error) = write_session_file_atomically(Path::new(path), replacement) {
            let mut rollback_failed = false;
            for (rollback_path, original, _) in originals.iter().take(replaced) {
                if write_session_file_atomically(Path::new(rollback_path), original).is_err() {
                    rollback_failed = true;
                }
            }
            if rollback_failed {
                anyhow::bail!(
                    "vault key rotation failed and automatic rollback was incomplete; encrypted recovery copies are in {backup_dir}"
                );
            }
            anyhow::bail!(
                "vault key rotation failed; prior records were restored and encrypted recovery copies remain in {backup_dir}: {error}"
            );
        }
        replaced += 1;
    }
    Ok((replaced, backup_dir))
}

fn session_vault_path(provider: &str) -> Result<String> {
    let legacy = session_path(provider)?;
    Ok(Path::new(&legacy)
        .with_extension("vault")
        .to_string_lossy()
        .into_owned())
}

fn remove_legacy_session_snapshot(provider: &str) -> Result<()> {
    let legacy_path = session_path(provider)?;
    match std::fs::remove_file(&legacy_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn parse_vault_key(encoded: &str) -> Result<Zeroizing<[u8; 32]>> {
    if encoded.len() != 64 || !encoded.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        anyhow::bail!("vault key must be exactly 64 hexadecimal characters");
    }
    let mut key = [0_u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16)
            .map_err(|_| anyhow::anyhow!("vault key contains invalid hexadecimal"))?;
    }
    Ok(Zeroizing::new(key))
}

fn vault_key_from_env() -> Result<Zeroizing<[u8; 32]>> {
    let encoded = Zeroizing::new(std::env::var("CHAT_GATEWAY_VAULT_KEY").map_err(|_| {
        anyhow::anyhow!("CHAT_GATEWAY_VAULT_KEY is required for encrypted session storage")
    })?);
    parse_vault_key(&encoded).map_err(|_| {
        anyhow::anyhow!("CHAT_GATEWAY_VAULT_KEY must be exactly 64 hexadecimal characters")
    })
}

fn vault_aad(provider: &str) -> String {
    format!("chat-gateway/session-vault/v1/{provider}")
}

fn encrypt_session_snapshot(
    file: &SessionFile,
    provider: &str,
    key_bytes: &[u8; 32],
) -> Result<Vec<u8>> {
    const MAGIC: &[u8; 6] = b"CGSV1\0";
    let mut nonce_bytes = [0_u8; 12];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("could not generate session-vault nonce"))?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, key_bytes)
        .map_err(|_| anyhow::anyhow!("invalid AES-256-GCM vault key"))?;
    let key = aead::LessSafeKey::new(unbound);
    let mut ciphertext = Zeroizing::new(serde_json::to_vec(file)?);
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce_bytes),
        aead::Aad::from(vault_aad(provider).as_bytes()),
        &mut *ciphertext,
    )
    .map_err(|_| anyhow::anyhow!("could not encrypt session snapshot"))?;
    let mut output = Vec::with_capacity(MAGIC.len() + nonce_bytes.len() + ciphertext.len());
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

fn decrypt_session_snapshot(
    bytes: &[u8],
    provider: &str,
    key_bytes: &[u8; 32],
) -> Result<SessionFile> {
    const MAGIC: &[u8; 6] = b"CGSV1\0";
    const NONCE_LEN: usize = 12;
    const TAG_LEN: usize = 16;
    if bytes.len() < MAGIC.len() + NONCE_LEN + TAG_LEN || !bytes.starts_with(MAGIC) {
        anyhow::bail!("invalid encrypted session-vault envelope");
    }
    let nonce_start = MAGIC.len();
    let nonce_end = nonce_start + NONCE_LEN;
    let nonce_bytes: [u8; NONCE_LEN] = bytes[nonce_start..nonce_end]
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid session-vault nonce"))?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, key_bytes)
        .map_err(|_| anyhow::anyhow!("invalid AES-256-GCM vault key"))?;
    let key = aead::LessSafeKey::new(unbound);
    let mut ciphertext = Zeroizing::new(bytes[nonce_end..].to_vec());
    let plaintext = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce_bytes),
            aead::Aad::from(vault_aad(provider).as_bytes()),
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("session-vault authentication failed"))?;
    Ok(serde_json::from_slice(plaintext)?)
}

fn ensure_private_session_dir_sync(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Load a saved session file, if one exists, using the default file store.
pub fn load_session_file(provider: &str) -> Result<Option<SessionFile>> {
    if matches!(
        std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref(),
        Ok("vault")
    ) {
        VaultSessionStore.load(provider)
    } else {
        FileSessionStore.load(provider)
    }
}

fn load_session_file_from_path(path: &Path) -> Result<Option<SessionFile>> {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(serde_json::from_str(&data)?))
}

/// Conservative provider-domain allowlist for persisted authentication cookies.
/// Keep these families narrow: session files contain reusable credentials.
fn cookie_domain_allowed(provider: &str, domain: &str) -> bool {
    let domain = domain
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty() || domain.contains('/') || domain.contains(':') {
        return false;
    }
    let allowed: &[&str] = match provider {
        "qwen" => &["qwen.ai", "aliyun.com"],
        "metaai" => &["meta.ai", "facebook.com", "fbcdn.net"],
        "claude" => &["claude.ai", "anthropic.com"],
        "chatgpt" => &["chatgpt.com", "openai.com"],
        "gemini" => &["gemini.google.com", "google.com", "googleusercontent.com"],
        "grok" => &["grok.com", "x.com", "twitter.com", "x.ai"],
        "mistral" => &["mistral.ai"],
        "perplexity" => &["perplexity.ai"],
        _ => return false,
    };
    allowed
        .iter()
        .any(|base| domain == *base || domain.ends_with(&format!(".{base}")))
}

/// Validate cookie fields from disk before interpolating them into a Set-Cookie-style string.
/// Session files can be stale or manually modified, so domain validation alone is insufficient.
fn restored_cookie_fields_safe(cookie: &Cookie) -> bool {
    let valid_name = !cookie.name.is_empty()
        && cookie
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    let valid_value = cookie
        .value
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b'"' | b',' | b';' | b'\\'));
    let valid_path = cookie.path.starts_with('/')
        && !cookie
            .path
            .bytes()
            .any(|b| b.is_ascii_control() || b == b';');
    valid_name && valid_value && valid_path
}
fn parse_local_storage_snapshot(
    value: serde_json::Value,
) -> Result<std::collections::HashMap<String, String>> {
    serde_json::from_value(value)
        .map_err(|error| anyhow::anyhow!("decoding provider localStorage snapshot: {error}"))
}

/// Ensure the directory holding reusable provider credentials is private.
/// Existing directories are hardened too; relying only on the process umask is
/// insufficient because it varies by deployment environment.
async fn ensure_private_session_dir(path: &Path) -> Result<()> {
    tokio::fs::create_dir_all(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}
/// Last successful session writes, shared by login completion and post-send
/// refreshes so rapid provider activity cannot churn credential files.
static LAST_SESSION_WRITE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
const SESSION_WRITE_INTERVAL: Duration = Duration::from_secs(60);

fn session_write_is_rate_limited(provider: &str, now: Instant) -> bool {
    LAST_SESSION_WRITE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|writes| writes.get(provider).copied())
        .is_some_and(|last| now.duration_since(last) < SESSION_WRITE_INTERVAL)
}

fn record_session_write(provider: &str) {
    if let Ok(mut writes) = LAST_SESSION_WRITE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        writes.insert(provider.to_string(), Instant::now());
    }
}

fn session_state_hash(file: &SessionFile) -> Result<String> {
    // Ignore ts and normalize ordering so a cookie jar with unchanged values
    // produces the same fingerprint across snapshots.
    let local_storage: BTreeMap<&String, &String> = file.local_storage.iter().collect();
    let mut cookies = file.cookies.clone();
    cookies.sort_by(|left, right| {
        (&left.domain, &left.name, &left.path, &left.value).cmp(&(
            &right.domain,
            &right.name,
            &right.path,
            &right.value,
        ))
    });
    let serialized = serde_json::to_vec(&(local_storage, cookies))?;
    let digest = Sha256::digest(serialized);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn capture_session(browser: &Browser, page: &Page, provider: &str) -> Result<SessionFile> {
    let ls_value = page.evaluate("(function(){ const o = {}; for (let i = 0; i < localStorage.length; i++) { const k = localStorage.key(i); o[k] = localStorage.getItem(k); } return o; })()");
    let local_storage = parse_local_storage_snapshot(ls_value)?;
    // Obscura's cookie store is browser-wide. Persist only cookies in the
    // provider's known domain family; otherwise saving one provider can copy
    // unrelated logged-in sessions into every other provider's session file.
    let cookies: Vec<Cookie> = browser
        .cookies()
        .get_all()
        .into_iter()
        .filter(|cookie| cookie_domain_allowed(provider, &cookie.domain))
        .map(Cookie::from)
        .collect();
    Ok(SessionFile {
        local_storage,
        cookies,
        ts: chrono::Utc::now().to_rfc3339(),
    })
}

/// Capture cookies (from the browser's CookieStore) and localStorage into
/// sessions/<provider>.json. Login completion deliberately forces a save.
pub async fn save_session(browser: &Browser, page: &Page, provider: &str) -> Result<()> {
    let file = capture_session(browser, page, provider).await?;
    write_session_snapshot(file, provider).await
}

/// Persist a post-send snapshot only when the provider-scoped session changed.
/// Writes are atomic and capped at one per provider per minute. A skipped
/// changed snapshot will be retried after the interval on the next successful send.
pub async fn save_session_if_changed(browser: &Browser, page: &Page, provider: &str) -> Result<()> {
    if matches!(
        std::env::var("CHAT_GATEWAY_SESSION_STORE").as_deref(),
        Ok("vault")
    ) {
        VaultSessionStore
            .save_if_changed(browser, page, provider)
            .await
    } else {
        FileSessionStore
            .save_if_changed(browser, page, provider)
            .await
    }
}

async fn save_session_if_changed_file(
    browser: &Browser,
    page: &Page,
    provider: &str,
) -> Result<()> {
    let file = capture_session(browser, page, provider).await?;
    let current_hash = session_state_hash(&file)?;
    let destination = session_path(provider)?;
    let previous_hash = std::fs::read(&destination)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SessionFile>(&bytes).ok())
        .and_then(|previous| session_state_hash(&previous).ok());

    if previous_hash.as_deref() == Some(current_hash.as_str()) {
        return Ok(());
    }
    if session_write_is_rate_limited(provider, Instant::now()) {
        tracing::debug!(
            provider,
            "session changed but snapshot write is rate-limited"
        );
        return Ok(());
    }
    write_session_snapshot(file, provider).await
}

async fn write_session_snapshot(file: SessionFile, provider: &str) -> Result<()> {
    ensure_private_session_dir(Path::new("sessions")).await?;
    let destination = session_path(provider)?;
    let serialized = serde_json::to_vec_pretty(&file)?;

    // Session files contain reusable authentication cookies. Create them
    // privately on Unix and replace the destination atomically.
    write_session_file_atomically(Path::new(&destination), &serialized)?;
    record_session_write(provider);
    tracing::info!(
        provider = provider,
        cookies = file.cookies.len(),
        "session saved"
    );
    Ok(())
}

fn write_session_file_atomically(destination: &Path, serialized: &[u8]) -> Result<()> {
    let temp_path = format!(
        "{}.tmp-{}-{}",
        destination.display(),
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Do not remove the path if create_new fails: in that case the temp path
    // may belong to another process and was not created by this invocation.
    let mut output = options.open(&temp_path)?;
    let write_result = (|| -> Result<()> {
        output.write_all(serialized)?;
        output.sync_all()?;
        Ok(())
    })();
    drop(output);

    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp_path, destination) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error.into());
    }
    Ok(())
}

fn ensure_cookie_restore_complete(failed_writes: usize) -> Result<()> {
    if failed_writes > 0 {
        anyhow::bail!("failed to restore {failed_writes} provider session cookie(s)");
    }
    Ok(())
}

#[cfg(test)]
mod cookie_restore_result_tests {
    use super::*;
    #[test]
    fn session_state_hash_ignores_timestamp_and_cookie_order() {
        let mut first = SessionFile {
            local_storage: [("token".to_string(), "value".to_string())]
                .into_iter()
                .collect(),
            cookies: vec![
                Cookie {
                    name: "b".into(),
                    value: "2".into(),
                    domain: ".claude.ai".into(),
                    path: "/".into(),
                    secure: true,
                    http_only: true,
                },
                Cookie {
                    name: "a".into(),
                    value: "1".into(),
                    domain: ".claude.ai".into(),
                    path: "/".into(),
                    secure: true,
                    http_only: true,
                },
            ],
            ts: "old".into(),
        };
        let first_hash = session_state_hash(&first).unwrap();
        first.ts = "new".into();
        first.cookies.reverse();
        assert_eq!(first_hash, session_state_hash(&first).unwrap());
    }

    #[test]
    fn session_state_hash_changes_when_cookie_value_rotates() {
        let mut file = SessionFile {
            local_storage: std::collections::HashMap::new(),
            cookies: vec![Cookie {
                name: "session".into(),
                value: "old".into(),
                domain: ".claude.ai".into(),
                path: "/".into(),
                secure: true,
                http_only: true,
            }],
            ts: "now".into(),
        };
        let old_hash = session_state_hash(&file).unwrap();
        file.cookies[0].value = "rotated".into();
        assert_ne!(old_hash, session_state_hash(&file).unwrap());
    }

    #[test]
    fn post_send_session_write_rate_limit_is_sixty_seconds() {
        let now = Instant::now();
        let mut writes = LAST_SESSION_WRITE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        writes.insert("test-provider".to_string(), now);
        drop(writes);
        assert!(session_write_is_rate_limited(
            "test-provider",
            now + Duration::from_secs(59)
        ));
        assert!(!session_write_is_rate_limited(
            "test-provider",
            now + Duration::from_secs(60)
        ));
        if let Ok(mut writes) = LAST_SESSION_WRITE.get().unwrap().lock() {
            writes.remove("test-provider");
        }
    }

    #[test]
    fn cookie_restore_succeeds_when_all_eligible_cookies_are_written() {
        assert!(ensure_cookie_restore_complete(0).is_ok());
    }

    #[test]
    fn cookie_restore_fails_when_any_cookie_cannot_be_restored() {
        let error = ensure_cookie_restore_complete(1).unwrap_err();
        assert!(error.to_string().contains("1 provider session cookie(s)"));
    }

    #[test]
    fn cookie_restore_reports_multiple_failures_without_cookie_contents() {
        let error = ensure_cookie_restore_complete(2).unwrap_err();
        assert!(error.to_string().contains("2 provider session cookie(s)"));
        assert!(!error.to_string().contains("secret"));
    }
}

/// Restore step 1 (BEFORE goto): put the saved cookies into the browser's
/// cookie store so the first navigation is already authenticated.
pub fn restore_cookies(browser: &Browser, file: &SessionFile, provider: &str) -> Result<()> {
    let store = browser.cookies();
    let mut failed_writes = 0usize;
    for c in &file.cookies {
        // Session files are local files and may have been edited or created by
        // an older version. Apply the same provider-domain allowlist at restore
        // time, not just when saving, to avoid cross-provider cookie injection.
        if !cookie_domain_allowed(provider, &c.domain) {
            tracing::warn!(provider, domain = %c.domain, "skipping cookie outside provider domain allowlist");
            continue;
        }
        if !restored_cookie_fields_safe(c) {
            failed_writes += 1;
            tracing::warn!(
                provider,
                "provider session cookie has invalid persisted fields"
            );
            continue;
        }
        let mut s = format!("{}={}", c.name, c.value);
        if !c.domain.is_empty() {
            s += &format!("; Domain={}", c.domain);
        }
        s += &format!(
            "; Path={}",
            if c.path.is_empty() {
                "/".to_string()
            } else {
                c.path.clone()
            }
        );
        if c.secure {
            s += "; Secure";
        }
        if c.http_only {
            s += "; HttpOnly";
        }
        let url = if c.domain.is_empty() {
            "https://localhost/".to_string()
        } else {
            format!(
                "https://{}{}",
                c.domain.trim_start_matches('.'),
                if c.path.is_empty() {
                    "/".to_string()
                } else {
                    c.path.clone()
                }
            )
        };
        if store.set(&s, &url).is_err() {
            failed_writes += 1;
            tracing::warn!(
                provider,
                cookie = %c.name,
                "provider session cookie restore failed"
            );
        }
    }
    ensure_cookie_restore_complete(failed_writes)
}

/// Restore step 2 (AFTER goto): localStorage is origin-scoped, so it can
/// only be injected once the page sits on the provider origin.
fn local_storage_restore_script(key: &str, value: &str) -> Option<String> {
    let key = serde_json::to_string(key).ok()?;
    let value = serde_json::to_string(value).ok()?;
    Some(format!("(() => {{ try {{ localStorage.setItem({key}, {value}); return true; }} catch (_error) {{ return false; }} }})()"))
}

pub fn restore_local_storage(page: &Page, file: &SessionFile) -> Result<()> {
    for (k, v) in &file.local_storage {
        let script = local_storage_restore_script(k, v).ok_or_else(|| {
            anyhow::anyhow!("failed to serialize provider localStorage restore script")
        })?;
        let restored = page.evaluate(&script);
        if restored != serde_json::Value::Bool(true) {
            return Err(anyhow::anyhow!(
                "failed to restore provider localStorage entry"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod local_storage_snapshot_tests {
    use super::*;

    #[test]
    fn parses_valid_local_storage_string_map() {
        let value = serde_json::json!({"theme": "dark", "locale": "en-US"});
        let parsed = parse_local_storage_snapshot(value).unwrap();
        assert_eq!(parsed.get("theme").map(String::as_str), Some("dark"));
        assert_eq!(parsed.get("locale").map(String::as_str), Some("en-US"));
    }

    #[test]
    fn rejects_invalid_local_storage_snapshot_instead_of_silently_emptying_it() {
        assert!(parse_local_storage_snapshot(serde_json::json!(["unexpected"])).is_err());
        assert!(parse_local_storage_snapshot(serde_json::json!({"theme": 42})).is_err());
    }
}

#[cfg(test)]
mod session_file_read_tests {
    use super::*;

    #[test]
    fn missing_session_file_returns_none() {
        let dir = std::env::temp_dir().join(format!(
            "chat-gateway-session-read-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result = load_session_file_from_path(&dir.join("missing.json"));
        std::fs::remove_dir_all(dir).unwrap();
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn malformed_session_file_returns_error() {
        let dir = std::env::temp_dir().join(format!(
            "chat-gateway-session-invalid-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("invalid.json");
        std::fs::write(&path, b"not-json").unwrap();
        let result = load_session_file_from_path(&path);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod local_storage_script_tests {
    use super::*;

    #[test]
    fn restore_script_uses_json_string_literals_for_keys_and_values() {
        let key = "key\"\\\n雪";
        let value = "\");alert(1);//\nline two\\tail";
        let script = local_storage_restore_script(key, value).unwrap();

        assert!(script.contains(&serde_json::to_string(key).unwrap()));
        assert!(script.contains(&serde_json::to_string(value).unwrap()));
        assert!(script.contains("localStorage.setItem("));
        assert!(script.contains("return true"));
        assert!(script.contains("return false"));
        assert!(script.contains("catch"));
        assert!(script.ends_with(")"));
    }

    #[test]
    fn restore_script_keeps_injection_shaped_values_inside_json_literal() {
        let value = "\");fetch('https://attacker.example')//";
        let script = local_storage_restore_script("session", value).unwrap();
        let literal = serde_json::to_string(value).unwrap();

        assert!(script.contains(&literal));
        assert_eq!(serde_json::from_str::<String>(&literal).unwrap(), value);
    }
}

#[cfg(test)]
mod cookie_scope_tests {
    use super::*;

    fn cookie(name: &str, value: &str, path: &str) -> Cookie {
        Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: ".chatgpt.com".to_string(),
            path: path.to_string(),
            secure: true,
            http_only: true,
        }
    }

    #[test]
    fn cookie_expiration_preserves_secure_cookie_prefix_requirements() {
        let mut host_cookie = cookie("__Host-session", "value", "/");
        host_cookie.domain = "chatgpt.com".to_string();
        assert!(cookie_expiration_header(&host_cookie)
            .unwrap()
            .contains("Max-Age=0; Path=/; Secure"));
        host_cookie.path = "/nested".to_string();
        assert!(cookie_expiration_header(&host_cookie).is_err());

        let mut secure_cookie = cookie("__Secure-session", "value", "/");
        secure_cookie.secure = false;
        assert!(cookie_expiration_header(&secure_cookie).is_err());
    }

    #[test]
    fn restored_cookie_fields_reject_injection_and_malformed_paths() {
        assert!(restored_cookie_fields_safe(&cookie(
            "__Secure-session",
            "abc_123-xyz",
            "/"
        )));
        assert!(!restored_cookie_fields_safe(&cookie(
            "bad;name", "value", "/"
        )));
        assert!(!restored_cookie_fields_safe(&cookie(
            "session",
            "value; Secure",
            "/"
        )));
        assert!(!restored_cookie_fields_safe(&cookie(
            "session",
            "value\r\nX-Evil: yes",
            "/"
        )));
        assert!(!restored_cookie_fields_safe(&cookie(
            "session",
            "value",
            "relative/path"
        )));
        assert!(!restored_cookie_fields_safe(&cookie(
            "session",
            "value",
            "/safe\r\nInjected: yes"
        )));
    }

    #[test]
    fn provider_cookie_allowlist_rejects_cross_provider_and_suffix_spoofing() {
        assert!(!cookie_domain_allowed("chatgpt", "claude.ai"));
        assert!(!cookie_domain_allowed("chatgpt", "notopenai.com"));
        assert!(!cookie_domain_allowed(
            "chatgpt",
            "openai.com.attacker.example"
        ));
        assert!(!cookie_domain_allowed("claude", "chatgpt.com"));
        assert!(!cookie_domain_allowed("unknown", "openai.com"));
    }

    #[test]
    fn persists_only_cookies_from_configured_provider_domain_families() {
        assert!(cookie_domain_allowed("chatgpt", ".chatgpt.com"));
        assert!(cookie_domain_allowed("chatgpt", "auth.openai.com"));
        assert!(!cookie_domain_allowed("chatgpt", "claude.ai"));
        assert!(!cookie_domain_allowed("claude", "chatgpt.com"));
        assert!(cookie_domain_allowed("gemini", ".google.com"));
        assert!(!cookie_domain_allowed("gemini", "attacker-google.com"));
    }

    #[test]
    fn rejects_unknown_provider_and_malformed_cookie_domains() {
        assert!(!cookie_domain_allowed("unknown", "example.com"));
        assert!(!cookie_domain_allowed("chatgpt", ""));
        assert!(!cookie_domain_allowed("chatgpt", "openai.com/path"));
        assert!(!cookie_domain_allowed("chatgpt", "openai.com:443"));
    }
}

#[cfg(all(test, unix))]
mod session_directory_permissions_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn private_session_directory_is_created_and_existing_mode_is_hardened() {
        let root = std::env::temp_dir().join(format!(
            "chat-gateway-session-dir-permissions-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();

        let existing = root.join("existing");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_session_dir(&existing).await.unwrap();
        assert_eq!(
            std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let new_dir = root.join("new");
        ensure_private_session_dir(&new_dir).await.unwrap();
        assert_eq!(
            std::fs::metadata(&new_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod atomic_session_write_tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chat-gateway-atomic-session-write-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_session_atomically_and_uses_private_unix_file_mode() {
        let dir = temp_dir();
        let destination = dir.join("chatgpt.json");
        write_session_file_atomically(&destination, br#"{"session":"new"}"#).unwrap();
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            br#"{"session":"new"}"#
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removes_temporary_session_file_when_atomic_replace_fails() {
        let dir = temp_dir();
        let destination = dir.join("destination-is-a-directory");
        std::fs::create_dir(&destination).unwrap();
        assert!(write_session_file_atomically(&destination, b"secret").is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod vault_tests {
    use super::*;

    fn sample_session() -> SessionFile {
        SessionFile {
            local_storage: HashMap::from([(
                "access_token".to_owned(),
                "sensitive-value".to_owned(),
            )]),
            cookies: vec![Cookie {
                name: "session".to_owned(),
                value: "cookie-secret".to_owned(),
                domain: "example.com".to_owned(),
                path: "/".to_owned(),
                secure: true,
                http_only: true,
            }],
            ts: "2026-10-27T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn vault_key_parser_accepts_only_32_byte_hex_keys() {
        assert_eq!(*parse_vault_key(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        assert!(parse_vault_key("abcd").is_err());
        assert!(parse_vault_key(&"xz".repeat(32)).is_err());
        assert!(parse_vault_key(&"ab".repeat(31)).is_err());
    }

    #[test]
    fn encrypted_vault_round_trips_without_storing_plaintext() {
        let session = sample_session();
        let key = [0x5a; 32];
        let encrypted = encrypt_session_snapshot(&session, "provider-a", &key).unwrap();
        assert!(encrypted.starts_with(b"CGSV1\0"));
        assert!(!encrypted
            .windows(b"sensitive-value".len())
            .any(|w| w == b"sensitive-value"));
        let restored = decrypt_session_snapshot(&encrypted, "provider-a", &key).unwrap();
        assert_eq!(
            restored.local_storage.get("access_token").unwrap(),
            "sensitive-value"
        );
        assert_eq!(restored.cookies[0].value, "cookie-secret");
    }

    #[test]
    fn encrypted_vault_rejects_tampering_and_wrong_provider() {
        let key = [0x21; 32];
        let encrypted = encrypt_session_snapshot(&sample_session(), "provider-a", &key).unwrap();
        assert!(decrypt_session_snapshot(&encrypted, "provider-b", &key).is_err());
        let mut tampered = encrypted;
        let last = tampered.len() - 1;
        tampered[last] ^= 0x80;
        assert!(decrypt_session_snapshot(&tampered, "provider-a", &key).is_err());
    }

    #[test]
    fn encrypted_vault_rejects_wrong_key_and_truncated_envelopes() {
        let encrypted =
            encrypt_session_snapshot(&sample_session(), "provider-a", &[0x44; 32]).unwrap();
        assert!(decrypt_session_snapshot(&encrypted, "provider-a", &[0x45; 32]).is_err());
        assert!(decrypt_session_snapshot(&encrypted[..10], "provider-a", &[0x44; 32]).is_err());
        let mut unknown_version = encrypted;
        unknown_version[5] = b'2';
        assert!(decrypt_session_snapshot(&unknown_version, "provider-a", &[0x44; 32]).is_err());
    }
}
