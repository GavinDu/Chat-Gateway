//! Narrow backend seam for canonical chat orchestration.
//!
//! The HTTP layer owns request validation, context construction, provider routing,
//! and response framing. A backend only executes a prepared provider request and
//! returns a stream of provider chunks. This lets tests substitute a deterministic
//! fake without constructing the thread-affine browser engine.
use crate::providers::ProviderChunk;
use anyhow::Result;
use std::{error::Error as StdError, fmt};
use tokio::sync::mpsc;

/// Stable error contract for backend startup failures. Provider stream errors
/// remain attached to individual chunks so an already-started stream can report
/// failure without changing the transport contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// Infrastructure failure; callers may try another configured provider.
    RetryableBackendUnavailable,
    /// Backend rejected or could not start the prepared request.
    BackendStartFailed(String),
}

impl GatewayError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RetryableBackendUnavailable)
    }

    fn from_backend_error(error: &anyhow::Error) -> Self {
        if error
            .downcast_ref::<crate::browser::pool::BrowserWorkerUnavailableError>()
            .is_some_and(|worker_error| worker_error.is_retryable())
        {
            Self::RetryableBackendUnavailable
        } else {
            Self::BackendStartFailed(error.to_string())
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetryableBackendUnavailable => {
                f.write_str("browser backend temporarily unavailable")
            }
            Self::BackendStartFailed(message) => f.write_str(message),
        }
    }
}

impl StdError for GatewayError {}

pub struct BackendRequest {
    pub provider: String,
    pub prompt: String,
    pub model: String,
    pub has_history: bool,
    pub thread_url: Option<String>,
}

#[async_trait::async_trait]
pub trait ChatBackend: Send + Sync {
    async fn send_stream(
        &self,
        request: BackendRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ProviderChunk>>, GatewayError>;
}

#[async_trait::async_trait]
impl ChatBackend for crate::browser::pool::ObscuraPool {
    async fn send_stream(
        &self,
        request: BackendRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ProviderChunk>>, GatewayError> {
        crate::browser::pool::ObscuraPool::send_stream(
            self,
            &request.provider,
            request.prompt,
            request.model,
            request.has_history,
            request.thread_url,
        )
        .await
        .map_err(|error| GatewayError::from_backend_error(&error))
    }
}

#[cfg(test)]
mod tests {
    use super::{BackendRequest, ChatBackend, GatewayError};
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
        ) -> std::result::Result<mpsc::Receiver<Result<ProviderChunk>>, super::GatewayError>
        {
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
    fn only_worker_unavailability_is_retryable() {
        assert!(GatewayError::RetryableBackendUnavailable.is_retryable());
        assert!(!GatewayError::BackendStartFailed("invalid provider".to_owned()).is_retryable());
    }

    #[tokio::test]
    async fn fake_backend_streams_chunks_without_browser_runtime() {
        let backend: Arc<dyn ChatBackend> = Arc::new(FakeBackend);
        let mut chunks = backend
            .send_stream(BackendRequest {
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
