use std::sync::Arc;
use chromiumoxide::Browser;
use futures::StreamExt;
use tokio::sync::Semaphore;
use crate::config::AppConfig;
use dashmap::DashMap;

#[derive(Clone)]
pub struct BrowserPool {
    browser: Arc<Browser>,
    semaphores: Arc<DashMap<String, Arc<Semaphore>>>,
    cfg: AppConfig,
}

impl BrowserPool {
    pub async fn connect(endpoint: &str, cfg: &AppConfig) -> anyhow::Result<Self> {
        // Obscura is CDP compatible - connect via ws endpoint
        let (browser, mut handler) = Browser::connect(endpoint).await?;

        tokio::spawn(async move {
            while let Some(res) = handler.next().await {
                if let Err(e) = res {
                    tracing::warn!("browser handler ended: {e}");
                    break;
                }
            }
        });

        let semaphores = DashMap::new();
        for p in &cfg.providers {
            if p.enabled {
                semaphores.insert(p.name.clone(), Arc::new(Semaphore::new(p.session_pool_size.max(1))));
            }
        }

        tracing::info!("connected to Obscura at {} with {} provider pools", endpoint, semaphores.len());
        Ok(Self {
            browser: Arc::new(browser),
            semaphores: Arc::new(semaphores),
            cfg: cfg.clone(),
        })
    }

    pub fn browser(&self) -> Arc<Browser> { self.browser.clone() }

    /// Acquire permit for provider - tracks queue time for performance trace
    pub async fn acquire(&self, provider: &str) -> anyhow::Result<BrowserPermit> {
        let start = std::time::Instant::now();
        let sem = self.semaphores.get(provider).map(|v| v.clone()).unwrap_or_else(|| Arc::new(Semaphore::new(2)));
        let available = sem.available_permits();
        if available == 0 {
            tracing::debug!(provider=provider, "provider queue: waiting for slot (pool exhausted)");
        }
        let permit = sem.acquire_owned().await?;
        let queue_ms = start.elapsed().as_millis();
        if queue_ms > 50 {
            tracing::debug!(provider=provider, queue_ms=%queue_ms, "provider queue wait");
        }
        Ok(BrowserPermit { _permit: permit, browser: self.browser.clone(), queue_ms })
    }
}

pub struct BrowserPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
    pub browser: Arc<Browser>,
    pub queue_ms: u128,
}
