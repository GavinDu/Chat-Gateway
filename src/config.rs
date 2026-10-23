use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub obscura: ObscuraConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub port: u16,
    #[serde(default = "default_true")]
    pub headless: bool,
    #[serde(default = "default_endpoint")]
    pub obscura_endpoint: String,
    #[serde(default)]
    pub chrome_profile_dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub name: String,
    pub enabled: bool,
    pub priority: u32,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_pool")]
    pub session_pool_size: usize,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MemoryConfig {
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_strategy")]
    pub strategy: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ObscuraConfig {
    #[serde(default = "default_true")]
    pub stealth: bool,
    #[serde(default = "default_true")]
    pub render: bool,
    #[serde(default)]
    pub allow_private_network: bool,
    #[serde(default = "default_binary")]
    pub binary: String,
}

impl AppConfig {
    pub fn load(p: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(p)?;
        let mut cfg: Self = serde_yaml::from_str(&raw)?;
        if let Ok(port) = std::env::var("PORT") {
            if let Ok(v) = port.parse() { cfg.server.port = v; }
        }
        cfg.providers.sort_by_key(|x| x.priority);
        Ok(cfg)
    }
}

fn default_true() -> bool { true }
fn default_endpoint() -> String { "ws://127.0.0.1:9222".to_string() }
fn default_timeout() -> u64 { 120 }
fn default_pool() -> usize { 2 }
fn default_max_tokens() -> usize { 8000 }
fn default_strategy() -> String { "sliding_window".to_string() }
fn default_binary() -> String { "obscura".to_string() }
impl Default for MemoryConfig {
    fn default() -> Self { Self { max_tokens: default_max_tokens(), strategy: default_strategy() } }
}
impl Default for ObscuraConfig {
    fn default() -> Self { Self { stealth: true, render: true, allow_private_network: true, binary: default_binary() } }
}
