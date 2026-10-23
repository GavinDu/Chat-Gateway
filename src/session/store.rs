use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::RwLock;
use serde::{Deserialize, Serialize};
use crate::{browser::{SessionData, PersistedCookie}, config::AppConfig};
use notify::{Watcher, RecursiveMode};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PersistedSession {
    pub cookies: Vec<PersistedCookie>,
    pub local_storage: HashMap<String, String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub user_agent: Option<String>,
}

pub struct SessionStore {
    base_dir: PathBuf,
    sessions: RwLock<HashMap<String, SessionData>>,
}

impl SessionStore {
    pub async fn load_all(cfg: &AppConfig) -> anyhow::Result<Self> {
        let base = PathBuf::from("sessions");
        tokio::fs::create_dir_all(&base).await.ok();
        let mut map = HashMap::new();
        for provider in &cfg.providers {
            let path = base.join(format!("{}.json", provider.name));
            if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                if let Ok(persisted) = serde_json::from_str::<PersistedSession>(&raw) {
                    let count = persisted.cookies.len();
                    map.insert(provider.name.clone(), SessionData {
                        cookies: persisted.cookies,
                        local_storage: persisted.local_storage,
                        timestamp: persisted.timestamp,
                    });
                    tracing::info!("loaded session for {} ({} cookies, {} localStorage)", provider.name, count, map.get(&provider.name).unwrap().local_storage.len());
                }
            } else {
                tracing::warn!("no session for {} - run dashboard login at http://localhost:8080 (will be stored as sessions/{}.json)", provider.name, provider.name);
            }
        }
        Ok(Self { base_dir: base, sessions: RwLock::new(map) })
    }

    pub async fn get(&self, provider: &str) -> Option<SessionData> {
        self.sessions.read().await.get(provider).cloned()
    }

    pub async fn apply_to_page(&self, provider: &str, page: &chromiumoxide::Page) -> anyhow::Result<()> {
        let sess = match self.get(provider).await {
            Some(s) => s,
            None => return Ok(()),
        };

        for cookie in &sess.cookies {
            if !cookie.http_only {
                let js = format!("try{{document.cookie={:?}}}catch(e){{}}", format!("{}={}", cookie.name, cookie.value));
                let _ = page.evaluate(js).await;
            }
            let cdp_js = format!(
                r#"try{{document.cookie={:?}}}catch(e){{}}"#,
                format!("{}={}; path={}", cookie.name, cookie.value, cookie.path)
            );
            let _ = page.evaluate(cdp_js).await;
        }

        for (k,v) in sess.local_storage {
            let script = format!("try{{localStorage.setItem({:?}, {:?})}}catch(e){{}}", k, v);
            let _ = page.evaluate(script).await;
        }

        tracing::debug!("applied {} cookies + localStorage for {provider}", sess.cookies.len());
        Ok(())
    }

    pub async fn capture_from_page(&self, provider: &str, page: &chromiumoxide::Page) -> anyhow::Result<()> {
        let ls_js = r#"() => { const o={}; for(let i=0;i<localStorage.length;i++){ const k=localStorage.key(i); o[k]=localStorage.getItem(k); } return o; }"#;
        let ls_value = page.evaluate(ls_js).await
            .ok()
            .and_then(|v| v.into_value::<HashMap<String, String>>().ok())
            .unwrap_or_default();

        let cookie_js = r#"() => document.cookie"#;
        let cookie_str = page.evaluate(cookie_js).await
            .ok()
            .and_then(|v| v.into_value::<String>().ok())
            .unwrap_or_default();

        let mut cookies = Vec::new();
        for part in cookie_str.split(';') {
            let part = part.trim();
            if part.is_empty() { continue; }
            if let Some((name, value)) = part.split_once('=') {
                cookies.push(PersistedCookie {
                    name: name.trim().to_string(),
                    value: value.trim().to_string(),
                    domain: "".to_string(),
                    path: "/".to_string(),
                    expires: 0.0,
                    http_only: false,
                    secure: false,
                    same_site: None,
                });
            }
        }

        if let Ok(cdp_cookies) = page.get_cookies().await {
            for c in cdp_cookies {
                if !cookies.iter().any(|pc| pc.name == c.name) {
                    cookies.push(PersistedCookie {
                        name: c.name.clone(),
                        value: c.value.clone(),
                        domain: c.domain.clone(),
                        path: c.path.clone(),
                        expires: c.expires as f64,
                        http_only: c.http_only,
                        secure: c.secure,
                        same_site: c.same_site.as_ref().map(|s| format!("{:?}", s)),
                    });
                }
            }
        }

        let persisted = PersistedSession {
            cookies: cookies.clone(),
            local_storage: ls_value.clone(),
            timestamp: chrono::Utc::now(),
            provider: provider.to_string(),
            user_agent: None,
        };

        let path = self.base_dir.join(format!("{}.json", provider));
        let json = serde_json::to_string_pretty(&persisted)?;
        tokio::fs::write(&path, json).await?;

        self.sessions.write().await.insert(provider.to_string(), SessionData {
            cookies,
            local_storage: ls_value,
            timestamp: chrono::Utc::now(),
        });

        tracing::info!("captured and saved session for {} to {:?}", provider, path);
        Ok(())
    }

    /// Watch sessions/*.json for changes and hot-reload — uses notify debouncer
    pub fn spawn_watcher(self: Arc<Self>) {
        let base = self.base_dir.clone();
        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
            let tx_clone = tx.clone();

            // Blocking watcher in separate thread
            std::thread::spawn(move || {
                let mut debouncer = match notify_debouncer_mini::new_debouncer(
                    Duration::from_millis(500),
                    None,
                    move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, _>| {
                        if let Ok(events) = res {
                            for ev in events {
                                let _ = tx_clone.blocking_send(ev.path);
                            }
                        }
                    }
                ) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("failed to create file watcher: {}", e);
                        return;
                    }
                };
                if let Err(e) = debouncer.watcher().watch(&base, RecursiveMode::NonRecursive) {
                    tracing::warn!("watch failed for {:?}: {}", base, e);
                }
                // Keep thread alive
                loop { std::thread::sleep(Duration::from_secs(10)); }
            });

            while let Some(path) = rx.recv().await {
                if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                    if !fname.ends_with(".json") { continue; }
                    let provider = fname.trim_end_matches(".json").to_string();
                    tokio::time::sleep(Duration::from_millis(100)).await; // wait for write to finish
                    if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                        if let Ok(persisted) = serde_json::from_str::<PersistedSession>(&raw) {
                            self.sessions.write().await.insert(provider.clone(), SessionData {
                                cookies: persisted.cookies,
                                local_storage: persisted.local_storage,
                                timestamp: persisted.timestamp,
                            });
                            tracing::info!("hot-reloaded session for {} from {:?}", provider, path);
                        }
                    }
                }
            }
        });
    }
}
