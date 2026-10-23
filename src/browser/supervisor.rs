use crate::config::ObscuraConfig;
use tokio::process::Command;
use std::time::Duration;

pub struct ObscuraSupervisor {
    _child: Option<tokio::process::Child>,
}

impl ObscuraSupervisor {
    pub async fn start(cfg: &ObscuraConfig) -> anyhow::Result<Self> {
        // Try to connect first - if endpoint alive, don't spawn
        if Self::probe("http://127.0.0.1:9222/json/version").await.is_ok() {
            tracing::info!("obscura already running at 9222");
            return Ok(Self { _child: None });
        }

        let mut args = vec!["serve".to_string(), "--port".to_string(), "9222".to_string()];
        if cfg.stealth { args.push("--stealth".to_string()); }
        if cfg.allow_private_network { args.push("--allow-private-network".to_string()); }

        tracing::info!(?args, "spawning obscura");

        let child = Command::new(&cfg.binary)
            .args(&args)
            .kill_on_drop(true)
            .spawn();

        match child {
            Ok(c) => {
                // wait for ready
                for _ in 0..30 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if Self::probe("http://127.0.0.1:9222/json/version").await.is_ok() {
                        tracing::info!("obscura ready");
                        return Ok(Self { _child: Some(c) });
                    }
                }
                tracing::warn!("obscura didn't become ready in time, continuing anyway");
                Ok(Self { _child: Some(c) })
            }
            Err(e) => {
                tracing::warn!("could not spawn obscura binary '{}': {}, assuming external", cfg.binary, e);
                Ok(Self { _child: None })
            }
        }
    }

    async fn probe(url: &str) -> anyhow::Result<()> {
        let resp = reqwest::get(url).await?;
        if resp.status().is_success() { Ok(()) } else { anyhow::bail!("bad status") }
    }
}
