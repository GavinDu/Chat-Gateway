//! Request execution seam between the HTTP handler and provider backends.
//!
//! The service owns backend dispatch and common event-stream consumption so
//! streaming and non-streaming handlers share the same provider event contract.
use crate::chat_backend::{BackendRequest, ChatBackend, ChatEvent, GatewayError};
use anyhow::Result;
use std::{sync::Arc, time::{Duration, Instant}};
use tokio::sync::mpsc;

pub struct ChatService {
    backend: Arc<dyn ChatBackend>,
}

/// A fully consumed provider response. Errors retain any partial content so
/// callers can enforce the rule that partial turns are never persisted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CollectedResponse {
    pub content: String,
    pub first_event_elapsed: Option<Duration>,
    pub error: Option<GatewayError>,
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

    /// Consume a provider stream for a non-SSE response using the same event
    /// semantics as the streaming path. The caller supplies the attempt start
    /// instant so TTFT includes backend startup and first-event wait.
    pub async fn collect(
        &self,
        mut events: mpsc::Receiver<Result<ChatEvent, GatewayError>>,
        attempt_started: Instant,
    ) -> CollectedResponse {
        let mut collected = CollectedResponse::default();
        while let Some(item) = events.recv().await {
            match item {
                Ok(event) if event.done => break,
                Ok(event) => {
                    if collected.first_event_elapsed.is_none() {
                        collected.first_event_elapsed = Some(attempt_started.elapsed());
                    }
                    collected.content.push_str(&event.content);
                }
                Err(error) => {
                    collected.error = Some(error);
                    break;
                }
            }
        }
        collected
    }
}

#[cfg(test)]
mod tests {
    use super::ChatService;
    use crate::chat_backend::{BackendRequest, ChatBackend, ChatEvent, GatewayError, ProviderId};
    use anyhow::Result;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct FakeBackend;

    #[async_trait::async_trait]
    impl ChatBackend for FakeBackend {
        async fn send_stream(
            &self,
            request: BackendRequest,
        ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError>
        {
            let (tx, rx) = mpsc::channel(2);
            tx.send(Ok(ChatEvent {
                content: format!("{}:{}", request.provider.as_str(), request.prompt),
                done: false,
            }))
            .await
            .expect("test receiver remains open");
            tx.send(Ok(ChatEvent { content: String::new(), done: true }))
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
                provider: ProviderId::new("claude").unwrap(),
                prompt: "test prompt".to_owned(),
                model: "test-model".to_owned(),
                has_history: true,
                thread_url: Some("https://provider.invalid/thread/1".to_owned()),
            })
            .await
            .expect("fake backend should accept the request");
        let event = events.recv().await.unwrap().unwrap();
        assert_eq!(event.content, "claude:test prompt");
        assert!(!event.done);
        assert!(events.recv().await.unwrap().unwrap().done);
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn collect_aggregates_until_done_and_records_first_event_latency() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let events = service.stream(BackendRequest {
            provider: ProviderId::new("claude").unwrap(),
            prompt: "hello".to_owned(),
            model: "test-model".to_owned(),
            has_history: false,
            thread_url: None,
        }).await.unwrap();
        let collected = service.collect(events, std::time::Instant::now()).await;
        assert_eq!(collected.content, "claude:hello");
        assert!(collected.error.is_none());
        assert!(collected.first_event_elapsed.is_some());
    }

    #[tokio::test]
    async fn collect_preserves_partial_content_but_reports_terminal_error() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(ChatEvent { content: "partial".into(), done: false })).await.unwrap();
        tx.send(Err(GatewayError::provider_failure("upstream failed"))).await.unwrap();
        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "partial");
        assert_eq!(collected.error.unwrap().message, "upstream failed");
    }

    #[tokio::test]
    async fn collect_treats_done_as_terminal_and_ignores_later_events() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(3);
        tx.send(Ok(ChatEvent { content: "answer".into(), done: false })).await.unwrap();
        tx.send(Ok(ChatEvent { content: String::new(), done: true })).await.unwrap();
        tx.send(Ok(ChatEvent { content: "must-not-append".into(), done: false })).await.unwrap();
        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "answer");
        assert!(collected.error.is_none());
    }
}
