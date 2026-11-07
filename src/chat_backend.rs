//! Canonical chat execution contract independent of the browser engine.
use crate::{
    browser::pool::{
        BrowserWorkerUnavailableError, ProviderBusyError, ProviderSessionExpiredError,
    },
    providers::ProviderChunk,
};
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

/// Canonical, redacted OpenAI-style response mapping. Backend error messages
/// are untrusted provider/browser diagnostics and must not be reflected to
/// clients. The active handler will migrate to this boundary incrementally.
impl axum::response::IntoResponse for GatewayError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::{header, HeaderValue, StatusCode};

        let (status, error_type, code, message) = match self.kind {
            GatewayErrorKind::InvalidRequest => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request",
                "invalid chat request",
            ),
            GatewayErrorKind::ModelNotFound => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
                "requested model is not available",
            ),
            GatewayErrorKind::ProviderBusy => (
                StatusCode::TOO_MANY_REQUESTS,
                "server_error",
                "provider_busy",
                "all eligible providers are busy; retry shortly",
            ),
            GatewayErrorKind::SessionExpired => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "provider_session_expired",
                "provider session expired; re-login required",
            ),
            GatewayErrorKind::Challenge => (
                StatusCode::FORBIDDEN,
                "provider_error",
                "provider_challenge",
                "provider challenge requires manual review",
            ),
            GatewayErrorKind::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "provider_error",
                "provider_timeout",
                "provider request timed out",
            ),
            GatewayErrorKind::Memory => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_error",
                "conversation could not be processed",
            ),
            GatewayErrorKind::ProviderFailure | GatewayErrorKind::BackendUnavailable => (
                StatusCode::BAD_GATEWAY,
                "provider_error",
                "provider_unavailable",
                "provider request failed",
            ),
        };
        let mut response = crate::api::api_error(status, error_type, code, message);
        if self.kind == GatewayErrorKind::ProviderBusy {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

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

/// Convert browser/backend failures into stable gateway error categories.
///
/// Provider auth expiry must not be flattened into a generic provider failure:
/// routing uses this distinction to stop retrying a dead session and request
/// an explicit owner re-login. Worker loss remains retryable infrastructure.
fn map_backend_error(error: anyhow::Error) -> GatewayError {
    if error
        .downcast_ref::<ProviderSessionExpiredError>()
        .is_some()
    {
        return GatewayError::session_expired("provider session expired; re-login required");
    }
    if error.downcast_ref::<ProviderBusyError>().is_some() {
        return GatewayError::provider_busy("provider is busy; retry shortly");
    }
    if error
        .downcast_ref::<BrowserWorkerUnavailableError>()
        .is_some_and(BrowserWorkerUnavailableError::is_retryable)
    {
        return GatewayError::backend_unavailable("browser worker is temporarily unavailable");
    }
    GatewayError::provider_failure(error.to_string())
}

/// Forward browser-provider events while observing downstream cancellation.
///
/// The downstream receiver is the HTTP/service consumer. If it is dropped,
/// stop reading the provider stream immediately so the underlying browser send
/// (and its provider permit) is released even when the provider is idle.
fn forward_provider_stream(
    mut source: mpsc::Receiver<Result<ProviderChunk>>,
    tx: mpsc::Sender<Result<ChatEvent, GatewayError>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tx.closed() => break,
                chunk = source.recv() => {
                    let Some(chunk) = chunk else {
                        break;
                    };
                    let event = chunk.map(ChatEvent::from).map_err(map_backend_error);
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
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
            .map_err(map_backend_error)
    }

    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        let source = crate::browser::pool::ObscuraPool::send_stream(
            self,
            request.provider.as_str(),
            request.prompt,
            request.model,
            request.has_history,
            request.thread_url,
        )
        .await
        .map_err(map_backend_error)?;
        let (tx, rx) = mpsc::channel(32);
        forward_provider_stream(source, tx);
        Ok(rx)
    }
}

/// Deterministic backend for exercising the real ChatService and, as the
/// HTTP handler is decomposed, the production router without an Obscura page.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{ChatBackend, ChatEvent, ChatRequest, GatewayError, ProviderId};
    use std::{collections::VecDeque, time::Duration};
    use tokio::sync::{mpsc, oneshot, Mutex};

    pub(crate) enum ScriptedSend {
        /// A backend rejected admission before a response stream exists.
        Reject(GatewayError),
        /// Emit events in order, including incomplete or errored streams.
        Events(Vec<Result<ChatEvent, GatewayError>>),
        /// Simulate delayed first content without blocking the HTTP worker.
        Delay(Duration, Vec<Result<ChatEvent, GatewayError>>),
        /// Simulate a backend that holds request admission open before
        /// returning its event channel. A caller timeout drops this future.
        DelayAdmission(Duration, Vec<Result<ChatEvent, GatewayError>>),
        /// Keep the response idle until the consumer drops it.
        Hold(oneshot::Sender<()>),
        /// Emit first content then hold the stream; notify on client drop.
        PartialThenHold(ChatEvent, oneshot::Sender<()>),
    }

    pub(crate) struct ScriptedBackend {
        sends: Mutex<VecDeque<ScriptedSend>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedBackend {
        pub(crate) fn new(sends: Vec<ScriptedSend>) -> Self {
            Self {
                sends: Mutex::new(sends.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        pub(crate) async fn observed_requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl ChatBackend for ScriptedBackend {
        async fn models(&self, provider: ProviderId) -> Result<Vec<String>, GatewayError> {
            Ok(vec![format!("{}-test-model", provider.as_str())])
        }

        async fn send_stream(
            &self,
            request: ChatRequest,
        ) -> Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
            self.requests.lock().await.push(request);
            let scripted = self
                .sends
                .lock()
                .await
                .pop_front()
                .expect("each test request needs a scripted backend response");
            match scripted {
                ScriptedSend::Reject(error) => Err(error),
                ScriptedSend::Events(events) => {
                    let (tx, rx) = mpsc::channel(events.len().max(1));
                    for event in events {
                        tx.send(event)
                            .await
                            .expect("scripted receiver is still held by this function");
                    }
                    Ok(rx)
                }
                ScriptedSend::Delay(delay, events) => {
                    let (tx, rx) = mpsc::channel(8);
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = tx.closed() => {}
                            _ = tokio::time::sleep(delay) => {
                                for event in events {
                                    if tx.send(event).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                    Ok(rx)
                }
                ScriptedSend::DelayAdmission(delay, events) => {
                    tokio::time::sleep(delay).await;
                    let (tx, rx) = mpsc::channel(events.len().max(1));
                    for event in events {
                        tx.send(event)
                            .await
                            .expect("scripted receiver is still held by this function");
                    }
                    Ok(rx)
                }
                ScriptedSend::Hold(cancelled) => {
                    let (tx, rx) = mpsc::channel::<Result<ChatEvent, GatewayError>>(1);
                    tokio::spawn(async move {
                        tx.closed().await;
                        let _ = cancelled.send(());
                    });
                    Ok(rx)
                }
                ScriptedSend::PartialThenHold(first, cancelled) => {
                    let (tx, rx) = mpsc::channel(1);
                    tokio::spawn(async move {
                        if tx.send(Ok(first)).await.is_ok() {
                            tx.closed().await;
                        }
                        let _ = cancelled.send(());
                    });
                    Ok(rx)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackendCapabilities, BackendId, ChatBackend, ChatEvent, ChatRequest, GatewayError,
        GatewayErrorKind, ProviderChunk, ProviderId,
    };
    use crate::browser::pool::{
        BrowserWorkerUnavailableError, ProviderBusyError, ProviderSessionExpiredError,
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

    #[test]
    fn backend_errors_preserve_session_expiry_and_worker_unavailability_types() {
        let expired = super::map_backend_error(anyhow::Error::new(ProviderSessionExpiredError));
        assert_eq!(expired.kind, GatewayErrorKind::SessionExpired);
        assert!(!expired.is_retryable());
        assert_eq!(
            expired.message,
            "provider session expired; re-login required"
        );

        let unavailable =
            super::map_backend_error(anyhow::Error::new(BrowserWorkerUnavailableError));
        assert_eq!(unavailable.kind, GatewayErrorKind::BackendUnavailable);
        assert!(unavailable.is_retryable());

        let busy = super::map_backend_error(anyhow::Error::new(ProviderBusyError));
        assert_eq!(busy.kind, GatewayErrorKind::ProviderBusy);
        assert!(busy.is_retryable());
        assert_eq!(busy.message, "provider is busy; retry shortly");

        let ordinary = super::map_backend_error(anyhow::anyhow!("provider rejected request"));
        assert_eq!(ordinary.kind, GatewayErrorKind::ProviderFailure);
        assert!(!ordinary.is_retryable());
    }

    #[tokio::test]
    async fn typed_errors_render_safe_openai_envelopes_with_expected_statuses() {
        use axum::{
            body::to_bytes,
            http::{header, StatusCode},
            response::IntoResponse,
        };

        let cases = [
            (
                GatewayErrorKind::InvalidRequest,
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                GatewayErrorKind::ModelNotFound,
                StatusCode::NOT_FOUND,
                "model_not_found",
            ),
            (
                GatewayErrorKind::ProviderBusy,
                StatusCode::TOO_MANY_REQUESTS,
                "provider_busy",
            ),
            (
                GatewayErrorKind::SessionExpired,
                StatusCode::UNAUTHORIZED,
                "provider_session_expired",
            ),
            (
                GatewayErrorKind::Challenge,
                StatusCode::FORBIDDEN,
                "provider_challenge",
            ),
            (
                GatewayErrorKind::Timeout,
                StatusCode::GATEWAY_TIMEOUT,
                "provider_timeout",
            ),
            (
                GatewayErrorKind::Memory,
                StatusCode::INTERNAL_SERVER_ERROR,
                "memory_error",
            ),
            (
                GatewayErrorKind::ProviderFailure,
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
            ),
            (
                GatewayErrorKind::BackendUnavailable,
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
            ),
        ];
        for (kind, expected_status, expected_code) in cases {
            let response = GatewayError::typed(
                kind,
                "sensitive: cookie=account-session; bearer=private-token",
                true,
            )
            .into_response();
            assert_eq!(
                response.status(),
                expected_status,
                "wrong status for {kind:?}"
            );
            assert_eq!(
                response.headers().get(header::RETRY_AFTER).is_some(),
                kind == GatewayErrorKind::ProviderBusy,
                "Retry-After must be exclusive to the provider-busy response",
            );
            let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["code"], expected_code);
            assert!(body["error"]["message"].as_str().is_some());
            assert!(!String::from_utf8_lossy(&bytes).contains("account-session"));
            assert!(!String::from_utf8_lossy(&bytes).contains("private-token"));
        }
    }

    #[tokio::test]
    async fn dropping_event_receiver_cancels_idle_provider_stream() {
        let (source_tx, source_rx) = mpsc::channel::<Result<ProviderChunk>>(1);
        let (event_tx, event_rx) = mpsc::channel(1);
        let forwarding = super::forward_provider_stream(source_rx, event_tx);

        // No provider event is sent: cancellation must not depend on the
        // upstream producing another chunk to notice that the consumer left.
        drop(event_rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), source_tx.closed())
            .await
            .expect("dropping the event receiver must close the provider stream");

        forwarding
            .await
            .expect("provider forwarding task should stop cleanly");
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
