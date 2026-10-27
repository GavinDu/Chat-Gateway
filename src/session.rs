use anyhow::Result;
use obscura_core::{Browser, Page};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
pub struct SessionFile {
    pub local_storage: std::collections::HashMap<String, String>,
    pub cookies: Vec<Cookie>,
    pub ts: String,
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

/// Load a saved session file, if one exists, using the default file store.
pub fn load_session_file(provider: &str) -> Result<Option<SessionFile>> {
    FileSessionStore.load(provider)
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
    FileSessionStore
        .save_if_changed(browser, page, provider)
        .await
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
