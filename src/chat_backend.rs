//! Canonical chat execution contract independent of the browser engine.
use crate::providers::ProviderChunk;
use anyhow::Result;
use std::{error::Error, fmt};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub provider: String,
    pub prompt: String,
    pub model: String,
    pub has_history: bool,
    pub thread_url: Option<String>,
}

/// Compatibility alias retained while the HTTP handler migrates.
pub type BackendRequest = ChatRequest;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayErrorKind {
    ProviderFailure,
    BackendUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    pub kind: GatewayErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl GatewayError {
    pub fn provider_failure(message: impl Into<String>) -> Self {
        Self {
            kind: GatewayErrorKind::ProviderFailure,
            message: message.into(),
            retryable: false,
        }
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
    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError>;
}

#[async_trait::async_trait]
impl ChatBackend for crate::browser::pool::ObscuraPool {
    async fn send_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        let mut source = crate::browser::pool::ObscuraPool::send_stream(
            self,
            &request.provider,
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
    use super::{ChatBackend, ChatEvent, ChatRequest, GatewayError, GatewayErrorKind};
    use anyhow::Result;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct FakeBackend;

    #[async_trait::async_trait]
    impl ChatBackend for FakeBackend {
        async fn send_stream(
            &self,
            request: ChatRequest,
        ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
            let (tx, rx) = mpsc::channel(2);
            tx.send(Ok(ChatEvent {
                content: format!("{}:{}", request.provider, request.prompt),
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
    async fn fake_backend_streams_canonical_events_without_browser_runtime() {
        let backend: Arc<dyn ChatBackend> = Arc::new(FakeBackend);
        let mut events = backend
            .send_stream(ChatRequest {
                provider: "chatgpt".to_owned(),
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
    fn gateway_error_retryability_is_explicit_and_typed() {
        let provider = GatewayError::provider_failure("provider rejected request");
        assert_eq!(provider.kind, GatewayErrorKind::ProviderFailure);
        assert!(!provider.retryable);
        let unavailable = GatewayError::backend_unavailable("worker restarting");
        assert_eq!(unavailable.kind, GatewayErrorKind::BackendUnavailable);
        assert!(unavailable.retryable);
    }
}
