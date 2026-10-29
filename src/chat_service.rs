//! Request execution seam between the HTTP handler and provider backends.
//!
//! Context planning, routing and HTTP/SSE framing remain in the API layer for now;
//! this service centralizes the prepared-request execution boundary so the handler
//! can be reduced without coupling tests to Obscura's browser runtime.
use crate::chat_backend::{BackendRequest, ChatBackend, GatewayError};
use crate::providers::ProviderChunk;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct ChatService {
    backend: Arc<dyn ChatBackend>,
}

impl ChatService {
    pub fn new(backend: Arc<dyn ChatBackend>) -> Self {
        Self { backend }
    }

    pub async fn stream(
        &self,
        request: BackendRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ProviderChunk>>, GatewayError> {
        self.backend.send_stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::ChatService;
    use crate::{
        chat_backend::{BackendRequest, ChatBackend},
        providers::ProviderChunk,
    };
    use anyhow::Result;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct FakeBackend;

    #[async_trait::async_trait]
    impl ChatBackend for FakeBackend {
        async fn send_stream(
            &self,
            request: BackendRequest,
        ) -> std::result::Result<mpsc::Receiver<Result<ProviderChunk>>, super::GatewayError> {
            let (tx, rx) = mpsc::channel(1);
            tx.send(Ok(ProviderChunk {
                content: format!("{}:{}", request.provider, request.prompt),
                done: true,
            }))
            .await
            .expect("test receiver remains open");
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn service_forwards_prepared_request_to_injected_backend() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let mut chunks = service
            .stream(BackendRequest {
                provider: "claude".to_owned(),
                prompt: "test prompt".to_owned(),
                model: "test-model".to_owned(),
                has_history: true,
                thread_url: Some("https://provider.invalid/thread/1".to_owned()),
            })
            .await
            .expect("fake backend should accept the request");
        let chunk = chunks.recv().await.unwrap().unwrap();
        assert_eq!(chunk.content, "claude:test prompt");
        assert!(chunk.done);
        assert!(chunks.recv().await.is_none());
    }
}
