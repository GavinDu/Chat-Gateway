use std::{collections::HashMap, path::PathBuf};
use tokio::sync::RwLock;
use crate::config::AppConfig;

pub struct ModelCache {
    path: PathBuf,
    inner: RwLock<HashMap<String, Vec<String>>>,
}

impl ModelCache {
    pub async fn load(cfg: &AppConfig) -> Self {
        let path = PathBuf::from("models-cache.json");
        let inner = if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            serde_json::from_str(&raw).unwrap_or_default()
        } else { HashMap::new() };
        Self { path, inner: RwLock::new(inner) }
    }

    pub async fn list(&self) -> HashMap<String, Vec<String>> { self.inner.read().await.clone() }

    pub async fn refresh_all(&self, cfg: &AppConfig, pool: &crate::browser::pool::BrowserPool, sessions: &crate::session::SessionStore) -> anyhow::Result<()> {
        // For each enabled provider, open page, call discover_models without sending message
        // Save combined with configured aliases, refresh at most once per 24h
        // If discovery fails, keep last successful catalog
        tracing::info!("refreshing model catalog...");
        let mut new_map = HashMap::new();
        for p in &cfg.providers {
            if !p.enabled { continue; }
            // TODO: real discovery
            new_map.insert(p.name.clone(), p.models.clone());
        }
        *self.inner.write().await = new_map.clone();
        let json = serde_json::to_string_pretty(&new_map)?;
        tokio::fs::write(&self.path, json).await.ok();
        Ok(())
    }
}
