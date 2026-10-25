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
    #[serde(default)]
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub keys: Vec<ApiKeyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiKeyConfig {
    pub name: String,
    /// Lowercase SHA-256 hex digest of the one-time-displayed secret.
    pub secret_hash: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub provider_allowlist: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub chrome_profile_dir: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            chrome_profile_dir: String::new(),
        }
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
        Self {
            max_tokens: default_max_tokens(),
            strategy: default_strategy(),
        }
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
        Self {
            stealth: true,
            storage_dir: None,
        }
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
    #[serde(default = "default_queue_timeout")]
    pub queue_timeout_seconds: u64,
    #[serde(default = "default_pool")]
    pub session_pool_size: usize,
    #[serde(default)]
    pub models: Vec<String>,
}

impl AppConfig {
    pub fn load(p: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(p).map_err(|e| {
            anyhow::anyhow!(
                "cannot read config {}: {e} (copy config.example.yaml to config.yaml)",
                p.display()
            )
        })?;
        let mut cfg: Self = serde_yaml::from_str(&raw)?;
        if let Ok(port) = std::env::var("PORT") {
            cfg.server.port = parse_port_override(&port)?;
        }
        cfg.validate()?;
        cfg.providers.sort_by_key(|x| x.priority);
        Ok(cfg)
    }

    /// Reject configuration that would otherwise be silently ignored or
    /// create zero-capacity pools / immediately-expiring provider requests.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.server.port == 0 {
            anyhow::bail!("server.port must be between 1 and 65535");
        }
        if self.memory.max_tokens == 0 {
            anyhow::bail!("memory.max_tokens must be greater than zero");
        }
        if self.memory.strategy != "sliding_window" {
            anyhow::bail!(
                "unsupported memory.strategy '{}'; supported value: sliding_window",
                self.memory.strategy
            );
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
        for key in &self.auth.keys {
            if key.name.trim().is_empty() {
                anyhow::bail!("auth key name must not be empty");
            }
            if key.secret_hash.len() != 64 || !key.secret_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                anyhow::bail!("auth key '{}' secret_hash must be a 64-character SHA-256 hex digest", key.name);
            }
            if key.scopes.is_empty() || key.scopes.iter().any(|scope| !matches!(scope.as_str(), "chat" | "admin")) {
                anyhow::bail!("auth key '{}' scopes must contain chat and/or admin", key.name);
            }
            if let Some(providers) = &key.provider_allowlist {
                if providers.iter().any(|provider| !PROVIDERS.contains(&provider.as_str())) {
                    anyhow::bail!("auth key '{}' has an unknown provider in provider_allowlist", key.name);
                }
            }
        }


        let mut seen = std::collections::HashSet::new();
        for provider in &self.providers {
            if !PROVIDERS.contains(&provider.name.as_str()) {
                anyhow::bail!("unknown provider '{}'", provider.name);
            }
            if !seen.insert(provider.name.as_str()) {
                anyhow::bail!("duplicate provider '{}'", provider.name);
            }
            if provider.timeout_seconds == 0 {
                anyhow::bail!(
                    "provider '{}' timeout_seconds must be greater than zero",
                    provider.name
                );
            }
            if provider.queue_timeout_seconds == 0 {
                anyhow::bail!(
                    "provider '{}' queue_timeout_seconds must be greater than zero",
                    provider.name
                );
            }
            if provider.session_pool_size == 0 {
                anyhow::bail!(
                    "provider '{}' session_pool_size must be greater than zero",
                    provider.name
                );
            }
        }
        Ok(())
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }
}

fn parse_port_override(value: &str) -> anyhow::Result<u16> {
    value.parse::<u16>().map_err(|error| {
        anyhow::anyhow!(
            "invalid PORT value '{value}': expected an integer from 1 to 65535 ({error})"
        )
    })
}

fn default_true() -> bool {
    true
}
fn default_port() -> u16 {
    8080
}
fn default_priority() -> u32 {
    99
}
fn default_timeout() -> u64 {
    120
}
fn default_queue_timeout() -> u64 {
    30
}
fn default_pool() -> usize {
    2
}
fn default_max_tokens() -> usize {
    8000
}
fn default_strategy() -> String {
    "sliding_window".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> AppConfig {
        serde_yaml::from_str(yaml).expect("test config parses")
    }

    #[test]
    fn rejects_malformed_port_environment_override() {
        assert!(parse_port_override("not-a-port").is_err());
        assert!(parse_port_override("65536").is_err());
        assert_eq!(parse_port_override("8081").unwrap(), 8081);
    }

    #[test]
    fn rejects_port_zero() {
        let cfg = config("server:\n  port: 0\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_supported_provider_configuration() {
        let cfg = config("memory:\n  max_tokens: 4000\nproviders:\n  - name: chatgpt\n    timeout_seconds: 30\n    session_pool_size: 1\n");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_unknown_or_duplicate_providers() {
        let unknown = config("providers:\n  - name: custom-provider\n");
        assert!(unknown.validate().is_err());

        let duplicate = config("providers:\n  - name: chatgpt\n  - name: chatgpt\n");
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn permits_a_safe_configuration_with_no_enabled_providers() {
        let none = config("providers:\n  - name: chatgpt\n    enabled: false\n");
        assert!(none.validate().is_ok());

        let empty = config("providers: []\n");
        assert!(empty.validate().is_ok());
    }

    #[test]
    fn rejects_zero_capacity_or_timeout() {
        let timeout = config("providers:\n  - name: chatgpt\n    timeout_seconds: 0\n");
        assert!(timeout.validate().is_err());

        let pool = config("providers:\n  - name: chatgpt\n    session_pool_size: 0\n");
        assert!(pool.validate().is_err());
    }

    #[test]
    fn rejects_zero_queue_timeout() {
        let cfg = config("providers:\n  - name: chatgpt\n    queue_timeout_seconds: 0\n");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn defaults_queue_timeout_to_thirty_seconds() {
        let cfg = config("providers:\n  - name: chatgpt\n");
        assert_eq!(cfg.provider("chatgpt").unwrap().queue_timeout_seconds, 30);
    }

    #[test]
    fn rejects_zero_memory_budget_and_unknown_strategy() {
        let budget = config("memory:\n  max_tokens: 0\n");
        assert!(budget.validate().is_err());

        let strategy = config("memory:\n  strategy: full_history\n");
        assert!(strategy.validate().is_err());
    }
}
