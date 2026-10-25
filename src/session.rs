use anyhow::Result;
use obscura_core::{Browser, Page};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::io::Write;

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
        Self { name: c.name, value: c.value, domain: c.domain, path: c.path, secure: c.secure, http_only: c.http_only }
    }
}

pub fn session_path(provider: &str) -> String {
    format!("sessions/{}.json", provider)
}

/// Load a saved session file, if one exists.
pub fn load_session_file(provider: &str) -> Result<Option<SessionFile>> {
    let path = session_path(provider);
    if !Path::new(&path).exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&data)?))
}

/// Capture cookies (from the browser's CookieStore - this is the v2 fix:
/// v1 only persisted localStorage) + localStorage into
/// sessions/<provider>.json.
pub async fn save_session(browser: &Browser, page: &Page, provider: &str) -> Result<()> {
    let ls_value = page.evaluate("(function(){ const o = {}; for (let i = 0; i < localStorage.length; i++) { const k = localStorage.key(i); o[k] = localStorage.getItem(k); } return o; })()");
    let local_storage: std::collections::HashMap<String, String> =
        serde_json::from_value(ls_value).unwrap_or_default();
    let cookies: Vec<Cookie> = browser.cookies().get_all().into_iter().map(Cookie::from).collect();
    let file = SessionFile { local_storage, cookies, ts: chrono::Utc::now().to_rfc3339() };
    tokio::fs::create_dir_all("sessions").await?;
    let destination = session_path(provider);
    let temp_path = format!("{}.tmp-{}-{}", destination, std::process::id(), uuid::Uuid::new_v4());
    let serialized = serde_json::to_vec_pretty(&file)?;

    // Session files contain reusable authentication cookies. Create them
    // privately on Unix and replace the destination atomically.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)?;
        output.write_all(&serialized)?;
        output.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        tokio::fs::write(&temp_path, &serialized).await?;
    }

    if let Err(error) = tokio::fs::rename(&temp_path, &destination).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error.into());
    }
    tracing::info!(provider = provider, cookies = file.cookies.len(), "session saved");
    Ok(())
}

/// Restore step 1 (BEFORE goto): put the saved cookies into the browser's
/// cookie store so the first navigation is already authenticated.
pub fn restore_cookies(browser: &Browser, file: &SessionFile) -> Result<()> {
    let store = browser.cookies();
    for c in &file.cookies {
        let mut s = format!("{}={}", c.name, c.value);
        if !c.domain.is_empty() {
            s += &format!("; Domain={}", c.domain);
        }
        s += &format!("; Path={}", if c.path.is_empty() { "/".to_string() } else { c.path.clone() });
        if c.secure { s += "; Secure"; }
        if c.http_only { s += "; HttpOnly"; }
        let url = if c.domain.is_empty() {
            "https://localhost/".to_string()
        } else {
            format!("https://{}{}", c.domain.trim_start_matches('.'), if c.path.is_empty() { "/".to_string() } else { c.path.clone() })
        };
        if let Err(e) = store.set(&s, &url) {
            tracing::debug!(cookie = c.name, "cookie restore failed: {e}");
        }
    }
    Ok(())
}

/// Restore step 2 (AFTER goto): localStorage is origin-scoped, so it can
/// only be injected once the page sits on the provider origin.
pub fn restore_local_storage(page: &Page, file: &SessionFile) {
    for (k, v) in &file.local_storage {
        let script = format!("try{{localStorage.setItem({:?}, {:?})}}catch(e){{}}", k, v);
        let _ = page.evaluate(&script);
    }
}

/// Back-compat combined restore for a page already on the provider
/// origin: cookies into the store, then localStorage into the page.
pub fn restore_session(browser: &Browser, page: &Page, provider: &str) -> Result<bool> {
    match load_session_file(provider)? {
        Some(file) => {
            restore_cookies(browser, &file)?;
            restore_local_storage(page, &file);
            Ok(true)
        }
        None => Ok(false),
    }
}
