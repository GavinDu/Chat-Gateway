//! Canonical chat execution contract independent of the browser engine.
use crate::providers::ProviderChunk;
use anyhow::Result;
use std::{error::Error, fmt};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub provider: ProviderId,
    pub prompt: String,
    pub model: String,
    pub has_history: bool,
    pub thread_url: Option<String>,
}

/// Compatibility alias retained while the HTTP handler migrates.
pub type BackendRequest = ChatRequest;

/// Stable provider identity used by routing, metrics, and backend diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        Some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity of an execution backend (for example, the browser relay).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackendId(String);

impl BackendId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        Some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Explicit feature contract; callers must not infer support from provider names.
#[allow(dead_code)] // Capability contract is consumed as provider adapters migrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackendCapabilities {
    pub streaming: bool,
    pub model_selection: bool,
    pub system_role: bool,
    pub addressable_threads: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEvent {
    pub content: String,
    pub done: bool,
}

impl From<ProviderChunk> for ChatEvent {
    fn from(chunk: ProviderChunk) -> Self {
        Self {
            content: chunk.content,
            done: chunk.done,
        }
    }
}

#[allow(dead_code)] // Typed variants are introduced ahead of handler conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayErrorKind {
    InvalidRequest,
    ModelNotFound,
    ProviderBusy,
    SessionExpired,
    Challenge,
    Timeout,
    Memory,
    ProviderFailure,
    BackendUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    pub kind: GatewayErrorKind,
    pub message: String,
    pub retryable: bool,
}

#[allow(dead_code)] // Constructors are consumed incrementally by adapter and handler migrations.
impl GatewayError {
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub fn provider_failure(message: impl Into<String>) -> Self {
        Self {
            kind: GatewayErrorKind::ProviderFailure,
            message: message.into(),
            retryable: false,
        }
    }

    #[allow(dead_code)] // Consumed as provider and HTTP boundaries migrate to typed errors.
    pub fn typed(kind: GatewayErrorKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }

    #[allow(dead_code)] // Used by the provider-slot error mapping migration.
    pub fn provider_busy(message: impl Into<String>) -> Self {
        Self::typed(GatewayErrorKind::ProviderBusy, message, true)
    }

    #[allow(dead_code)] // Used by the provider auth-state mapping migration.
    pub fn session_expired(message: impl Into<String>) -> Self {
        Self::typed(GatewayErrorKind::SessionExpired, message, false)
    }

    #[allow(dead_code)] // Used by the provider challenge-state mapping migration.
    pub fn challenge(message: impl Into<String>) -> Self {
        Self::typed(GatewayErrorKind::Challenge, message, false)
    }

    #[allow(dead_code)] // Used by the provider timeout mapping migration.
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::typed(GatewayErrorKind::Timeout, message, true)
    }

    #[allow(dead_code)] // Used by the worker-recovery path as it moves behind ChatBackend.
    pub fn backend_unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: GatewayErrorKind::BackendUnavailable,
            message: message.into(),
            retryable: true,
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for GatewayError {}

#[async_trait::async_trait]
pub trait ChatBackend: Send + Sync {
    /// Identifier for this execution backend (not the selected provider).
    fn id(&self) -> BackendId {
        BackendId::new("browser").expect("static backend id is valid")
    }

    /// Capabilities are explicit and conservative by default.
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
    }

    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError>;

    /// Discover models through the selected provider adapter.
    async fn models(&self, provider: ProviderId) -> std::result::Result<Vec<String>, GatewayError>;
}

#[async_trait::async_trait]
impl ChatBackend for crate::browser::pool::ObscuraPool {
    fn id(&self) -> BackendId {
        BackendId::new("browser").expect("static backend id is valid")
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            streaming: true,
            model_selection: true,
            system_role: false,
            addressable_threads: false,
        }
    }

    async fn models(&self, provider: ProviderId) -> std::result::Result<Vec<String>, GatewayError> {
        crate::browser::pool::ObscuraPool::discover(self, provider.as_str())
            .await
            .map_err(|error| {
                if error
                    .downcast_ref::<crate::browser::pool::BrowserWorkerUnavailableError>()
                    .is_some_and(|worker_error| worker_error.is_retryable())
                {
                    GatewayError::backend_unavailable(error.to_string())
                } else {
                    GatewayError::provider_failure(error.to_string())
                }
            })
    }

    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        let mut source = crate::browser::pool::ObscuraPool::send_stream(
            self,
            request.provider.as_str(),
            request.prompt,
            request.model,
            request.has_history,
            request.thread_url,
        )
        .await
        .map_err(|error| {
            if error
                .downcast_ref::<crate::browser::pool::BrowserWorkerUnavailableError>()
                .is_some_and(|worker_error| worker_error.is_retryable())
            {
                GatewayError::backend_unavailable(error.to_string())
            } else {
                GatewayError::provider_failure(error.to_string())
            }
        })?;
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            while let Some(chunk) = source.recv().await {
                let event = chunk
                    .map(ChatEvent::from)
                    .map_err(|error| GatewayError::provider_failure(error.to_string()));
                if tx.send(event).await.is_err() {
                    break;
                }
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackendCapabilities, BackendId, ChatBackend, ChatEvent, ChatRequest, GatewayError,
        GatewayErrorKind, ProviderId,
    };
    use anyhow::Result;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct FakeBackend;

    #[async_trait::async_trait]
    impl ChatBackend for FakeBackend {
        async fn models(
            &self,
            provider: ProviderId,
        ) -> std::result::Result<Vec<String>, GatewayError> {
            Ok(vec![format!("{}-test-model", provider.as_str())])
        }

        async fn send_stream(
            &self,
            request: ChatRequest,
        ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError>
        {
            let (tx, rx) = mpsc::channel(2);
            tx.send(Ok(ChatEvent {
                content: format!("{}:{}", request.provider.as_str(), request.prompt),
                done: false,
            }))
            .await
            .expect("receiver should be alive");
            tx.send(Ok(ChatEvent {
                content: String::new(),
                done: true,
            }))
            .await
            .expect("receiver should be alive");
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn fake_backend_exposes_provider_model_discovery_without_browser_runtime() {
        let backend = FakeBackend;
        let models = backend
            .models(ProviderId::new("chatgpt").unwrap())
            .await
            .expect("fake backend model discovery should succeed");
        assert_eq!(models, vec!["chatgpt-test-model"]);
    }

    #[tokio::test]
    async fn fake_backend_streams_canonical_events_without_browser_runtime() {
        let backend: Arc<dyn ChatBackend> = Arc::new(FakeBackend);
        let mut events = backend
            .send_stream(ChatRequest {
                provider: ProviderId::new("chatgpt").unwrap(),
                prompt: "hello".to_owned(),
                model: "test-model".to_owned(),
                has_history: false,
                thread_url: None,
            })
            .await
            .expect("fake backend should accept request");

        assert_eq!(
            events.recv().await.unwrap().unwrap(),
            ChatEvent {
                content: "chatgpt:hello".to_owned(),
                done: false,
            }
        );
        assert_eq!(
            events.recv().await.unwrap().unwrap(),
            ChatEvent {
                content: String::new(),
                done: true,
            }
        );
        assert!(events.recv().await.is_none());
    }

    #[test]
    fn provider_ids_and_capabilities_are_explicit() {
        assert_eq!(ProviderId::new("chat-gpt").unwrap().as_str(), "chat-gpt");
        assert!(ProviderId::new("../chatgpt").is_none());
        assert!(ProviderId::new("").is_none());
        assert_eq!(BackendId::new("browser").unwrap().as_str(), "browser");
        assert!(BackendId::new("../browser").is_none());
        assert_eq!(
            BackendCapabilities::default(),
            BackendCapabilities {
                streaming: false,
                model_selection: false,
                system_role: false,
                addressable_threads: false,
            }
        );
    }

    #[test]
    fn gateway_error_retryability_is_explicit_and_typed() {
        let busy = GatewayError::provider_busy("provider slot is occupied");
        assert_eq!(busy.kind, GatewayErrorKind::ProviderBusy);
        assert!(busy.is_retryable());
        let timeout = GatewayError::timeout("provider timed out");
        assert_eq!(timeout.kind, GatewayErrorKind::Timeout);
        assert!(timeout.is_retryable());
        let expired = GatewayError::session_expired("login required");
        assert_eq!(expired.kind, GatewayErrorKind::SessionExpired);
        assert!(!expired.is_retryable());

        let provider = GatewayError::provider_failure("provider rejected request");
        assert_eq!(provider.kind, GatewayErrorKind::ProviderFailure);
        assert!(!provider.retryable);
        let unavailable = GatewayError::backend_unavailable("worker restarting");
        assert_eq!(unavailable.kind, GatewayErrorKind::BackendUnavailable);
        assert!(unavailable.retryable);
    }
}
