use serde::Deserialize;
use std::path::Path;

/// Minimal YAML config, same shape as v1's config.example.yaml:
/// server / memory / obscura / providers (name, enabled, priority,
/// timeout_seconds, session_pool_size, models). PORT env overrides
/// server.port.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub obscura: ObscuraConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub headless: bool,
    #[serde(default)]
    pub chrome_profile_dir: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { port: default_port(), headless: true, chrome_profile_dir: String::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_strategy")]
    pub strategy: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { max_tokens: default_max_tokens(), strategy: default_strategy() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObscuraConfig {
    #[serde(default = "default_true")]
    pub stealth: bool,
    /// Persistent cookie/storage directory for the embedded browser
    /// (falls back to server.chrome_profile_dir when unset).
    #[serde(default)]
    pub storage_dir: Option<String>,
}

impl Default for ObscuraConfig {
    fn default() -> Self {
        Self { stealth: true, storage_dir: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_pool")]
    pub session_pool_size: usize,
    #[serde(default)]
    pub models: Vec<String>,
}

impl AppConfig {
    pub fn load(p: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(p).map_err(|e| {
            anyhow::anyhow!("cannot read config {}: {e} (copy config.example.yaml to config.yaml)", p.display())
        })?;
        let mut cfg: Self = serde_yaml::from_str(&raw)?;
        if let Ok(port) = std::env::var("PORT") {
            if let Ok(v) = port.parse() {
                cfg.server.port = v;
            }
        }
        cfg.providers.sort_by_key(|x| x.priority);
        Ok(cfg)
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }
}

fn default_true() -> bool { true }
fn default_port() -> u16 { 8080 }
fn default_priority() -> u32 { 99 }
fn default_timeout() -> u64 { 120 }
fn default_pool() -> usize { 2 }
fn default_max_tokens() -> usize { 8000 }
fn default_strategy() -> String { "sliding_window".to_string() }
