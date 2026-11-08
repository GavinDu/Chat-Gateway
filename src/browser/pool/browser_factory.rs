//! Embedded browser construction with existing storage and stealth defaults.
use crate::config::AppConfig;
use anyhow::Result;

pub(super) fn build_browser(cfg: &AppConfig) -> Result<obscura_core::Browser> {
    let mut builder = obscura_core::Browser::builder().stealth(cfg.obscura.stealth);
    if let Some(user_agent) = cfg
        .obscura
        .user_agent
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder = builder.user_agent(user_agent.to_owned());
    }
    let storage = cfg
        .obscura
        .storage_dir
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if cfg.server.chrome_profile_dir.is_empty() {
                None
            } else {
                Some(cfg.server.chrome_profile_dir.clone())
            }
        });
    if let Some(dir) = storage {
        builder = builder.storage_dir(dir);
    }
    Ok(builder.build()?)
}
