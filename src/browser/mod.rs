pub mod pool;
pub mod login;
mod supervisor;

pub use supervisor::ObscuraSupervisor;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCookie {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default)]
    pub expires: f64,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub same_site: Option<String>,
}

fn default_path() -> String { "/".to_string() }

#[derive(Debug, Clone)]
pub struct SessionData {
    pub cookies: Vec<PersistedCookie>,
    pub local_storage: std::collections::HashMap<String, String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}
