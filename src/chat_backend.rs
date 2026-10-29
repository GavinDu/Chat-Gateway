//! Narrow backend seam for canonical chat orchestration.
//!
//! The HTTP layer owns request validation, context construction, provider routing,
//! and response framing. A backend only executes a prepared provider request and
//! returns a stream of provider chunks. This lets tests substitute a deterministic
//! fake without constructing the thread-affine browser engine.
use crate::providers::ProviderChunk;
use anyhow::Result;
use std::{error::Error, fmt};
use tokio::sync::mpsc;

/// Canonical request passed to an execution backend after gateway routing/context preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub provider: String,
    pub prompt: String,
    pub model: String,
    pub has_history: bool,
    pub thread_url: Option<String>,
}

/// A provider-independent streamed event. The existing adapter chunk already has the
/// canonical content/done shape, so this alias avoids a needless allocation/copy.
pub type ChatEvent = ProviderChunk;

/// Typed failure before a stream is established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// Infrastructure is temporarily unavailable; routing may try another provider.
    RetryableWorkerUnavailable,
    /// A non-retryable backend setup failure.
    Backend(String),
}

impl GatewayError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RetryableWorkerUnavailable)
    }

    fn from_backend_error(error: anyhow::Error) -> Self {
        if error
            .downcast_ref::<crate::browser::pool::BrowserWorkerUnavailableError>()
            .is_some_and(|typed| typed.is_retryable())
        {
            Self::RetryableWorkerUnavailable
        } else {
            Self::Backend(error.to_string())
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetryableWorkerUnavailable => {
                f.write_str("browser worker unavailable; retry after recovery")
            }
            Self::Backend(message) => f.write_str(message),
        }
    }
}

impl Error for GatewayError {}

#[async_trait::async_trait]
pub trait ChatBackend: Send + Sync {
    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent>>, GatewayError>;
}

#[async_trait::async_trait]
impl ChatBackend for crate::browser::pool::ObscuraPool {
    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent>>, GatewayError> {
        crate::browser::pool::ObscuraPool::send_stream(
            self,
            &request.provider,
            request.prompt,
            request.model,
            request.has_history,
            request.thread_url,
        )
        .await
        .map_err(GatewayError::from_backend_error)
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatBackend, ChatRequest, GatewayError};
    use crate::providers::ProviderChunk;
    use anyhow::Result;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct FakeBackend;

    #[async_trait::async_trait]
    impl ChatBackend for FakeBackend {
        async fn send_stream(
            &self,
            request: BackendRequest,
        ) -> std::result::Result<mpsc::Receiver<Result<ProviderChunk>>, GatewayError> {
            let (tx, rx) = mpsc::channel(2);
            tx.send(Ok(ProviderChunk {
                content: format!("{}:{}", request.provider, request.prompt),
                done: false,
            }))
            .await
            .expect("receiver should be alive");
            tx.send(Ok(ProviderChunk {
                content: String::new(),
                done: true,
            }))
            .await
            .expect("receiver should be alive");
            Ok(rx)
        }
    }

    #[test]
    fn retryable_worker_failure_is_explicit_in_the_canonical_contract() {
        let error = GatewayError::RetryableWorkerUnavailable;
        assert!(error.is_retryable());
        assert_eq!(
            error.to_string(),
            "browser worker unavailable; retry after recovery"
        );
        assert!(!GatewayError::Backend("bad request".to_owned()).is_retryable());
    }

    #[tokio::test]
    async fn fake_backend_streams_chunks_without_browser_runtime() {
        let backend: Arc<dyn ChatBackend> = Arc::new(FakeBackend);
        let mut chunks = backend
            .send_stream(ChatRequest {
                provider: "chatgpt".to_owned(),
                prompt: "hello".to_owned(),
                model: "test-model".to_owned(),
                has_history: false,
                thread_url: None,
            })
            .await
            .expect("fake backend should accept request");

        let first = chunks.recv().await.unwrap().unwrap();
        assert_eq!(first.content, "chatgpt:hello");
        assert!(!first.done);
        let done = chunks.recv().await.unwrap().unwrap();
        assert!(done.done);
        assert!(chunks.recv().await.is_none());
    }
}
