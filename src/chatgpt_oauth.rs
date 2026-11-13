//! Supported ChatGPT-plan OAuth and public Responses transport.
//! Browser cookies are deliberately kept in a separate vault namespace.
mod auth;
mod backend;
mod protocol;
pub(crate) mod storage;
pub(crate) use auth::{connect, logout, status};
pub(crate) use backend::RoutedBackend;

use crate::chat_backend::{GatewayError, GatewayErrorKind};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};
use zeroize::Zeroize;

const AUTH_ORIGIN: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
const MAX_JSON: usize = 1_048_576;

// Never derive Debug for credentials or authorization attempts.
#[derive(Clone, Serialize, Deserialize)]
struct Credential {
    authorization_id: String,
    issuer: String,
    subject: String,
    email: String,
    access_token: String,
    refresh_token: String,
    id_token: String,
    scopes: Vec<String>,
    expires_at: u64,
}
impl Credential {
    fn plan_enabled(&self) -> bool {
        plan_scopes(&self.scopes) && !self.access_token.is_empty() && !self.refresh_token.is_empty()
    }
}
fn plan_scopes(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == DIRECT_SCOPE) && scopes.iter().any(|s| s == "resource.invoke")
}
impl Drop for Credential {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
        self.subject.zeroize();
        self.email.zeroize();
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Registration {
    client_id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    issuer: String,
    credential: Option<Credential>,
}
#[derive(Serialize, Deserialize)]
struct Store {
    host_id: String,
    active: String,
    profiles: BTreeMap<String, Registration>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            host_id: format!("urn:uuid:{}", uuid::Uuid::new_v4()),
            active: "default".into(),
            profiles: BTreeMap::new(),
        }
    }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn failure(message: &'static str) -> GatewayError {
    GatewayError::provider_failure(message)
}
fn login_required() -> GatewayError {
    GatewayError::session_expired("ChatGPT account connection required; run chatgpt-connect")
}
fn http_client() -> Result<reqwest::Client, GatewayError> {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(150))
        .build()
        .map_err(|_| failure("ChatGPT HTTP client unavailable"))
}
async fn bounded_json(response: reqwest::Response) -> Result<serde_json::Value, GatewayError> {
    use futures_util::StreamExt;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_JSON as u64)
    {
        return Err(failure("ChatGPT response exceeds the size limit"));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| failure("ChatGPT response read failed"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_JSON {
            return Err(failure("ChatGPT response exceeds the size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| failure("ChatGPT response is invalid"))
}
fn status_error(status: reqwest::StatusCode) -> GatewayError {
    match status.as_u16() {
        401 => login_required(),
        403 => GatewayError::typed(
            GatewayErrorKind::ProviderAccessDenied,
            "ChatGPT plan access is restricted by account, policy, or region",
            false,
        ),
        429 => GatewayError::typed(
            GatewayErrorKind::ProviderUsageLimited,
            "ChatGPT usage limit reached",
            false,
        ),
        500 | 502 | 503 | 504 => {
            GatewayError::backend_unavailable("ChatGPT service temporarily unavailable")
        }
        _ => failure("ChatGPT service request failed"),
    }
}

/// A fresh explicit grant permits recovery; routine access-token rotation does
/// not erase a failed generation or claim a successful completion.
pub(crate) fn start_monitor(state: &std::sync::Arc<crate::AppState>) {
    if !state
        .cfg
        .provider("chatgpt")
        .is_some_and(|p| p.enabled && p.uses_oauth())
    {
        return;
    }
    let sessions = state.provider_sessions.clone();
    tokio::spawn(async move {
        let mut previous: Option<Option<String>> = None;
        loop {
            let current = async {
                let guard = storage::lock().await?;
                let store = storage::load(&guard)?;
                Ok::<_, GatewayError>(
                    store
                        .profiles
                        .get(&store.active)
                        .and_then(|p| p.credential.as_ref())
                        .filter(|c| c.plan_enabled())
                        .map(|c| c.authorization_id.clone()),
                )
            }
            .await;
            if let Ok(current) = current {
                if previous.as_ref() != Some(&current) {
                    if current.is_some() {
                        let _ = sessions.reset_for_retry("chatgpt");
                        sessions.record_probe_success("chatgpt");
                    } else {
                        sessions.record_needs_login("chatgpt");
                    }
                    previous = Some(current);
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

pub(crate) async fn catalog(
    config: crate::config::ProviderConfig,
) -> Result<Vec<String>, GatewayError> {
    backend::OAuthBackend::new(config)?.models().await
}

pub(crate) fn linked_snapshot() -> anyhow::Result<bool> {
    storage::linked_snapshot().map_err(Into::into)
}
