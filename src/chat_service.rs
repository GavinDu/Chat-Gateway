//! Request execution seam between the HTTP handler and provider backends.
//!
//! Context planning, routing and HTTP/SSE framing remain in the API layer for now;
//! this service centralizes prepared-request execution without coupling tests to
//! Obscura's browser runtime.
use crate::chat_backend::{BackendRequest, ChatBackend, ChatEvent, GatewayError};
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
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        let backend_id = self.backend.id();
        let capabilities = self.backend.capabilities();
        tracing::debug!(
            backend = backend_id.as_str(),
            streaming = capabilities.streaming,
            model_selection = capabilities.model_selection,
            system_role = capabilities.system_role,
            addressable_threads = capabilities.addressable_threads,
            "executing request through chat backend"
        );
        self.backend.send_stream(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::ChatService;
    use crate::chat_backend::{BackendRequest, ChatBackend, ChatEvent, GatewayError};
    use anyhow::Result;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct FakeBackend;

    #[async_trait::async_trait]
    impl ChatBackend for FakeBackend {
        async fn models(
            &self,
            _provider: crate::chat_backend::ProviderId,
        ) -> std::result::Result<Vec<String>, GatewayError> {
            Ok(vec!["test-model".to_owned()])
        }

        async fn send_stream(
            &self,
            request: BackendRequest,
        ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError>
        {
            let (tx, rx) = mpsc::channel(1);
            tx.send(Ok(ChatEvent {
                content: format!("{}:{}", request.provider.as_str(), request.prompt),
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
        let mut events = service
            .stream(BackendRequest {
                provider: crate::chat_backend::ProviderId::new("claude").unwrap(),
                prompt: "test prompt".to_owned(),
                model: "test-model".to_owned(),
                has_history: true,
                thread_url: Some("https://provider.invalid/thread/1".to_owned()),
            })
            .await
            .expect("fake backend should accept the request");
        let event = events.recv().await.unwrap().unwrap();
        assert_eq!(event.content, "claude:test prompt");
        assert!(event.done);
        assert!(events.recv().await.is_none());
    }
}
