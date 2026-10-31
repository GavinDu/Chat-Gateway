//! Request execution seam between the HTTP handler and provider backends.
//!
//! The service owns backend dispatch and common event-stream consumption so
//! streaming and non-streaming handlers share the same provider event contract.
use crate::chat_backend::{BackendRequest, ChatBackend, ChatEvent, GatewayError};
use anyhow::Result;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
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
        let mut saw_terminal_completion = false;
        while let Some(item) = events.recv().await {
            match item {
                Ok(event) if event.done => {
                    saw_terminal_completion = true;
                    break;
                }
                Ok(event) => {
                    // TTFT is the time to the first actual content, not an
                    // empty heartbeat or metadata-only event.
                    if !event.content.is_empty() {
                        if collected.first_event_elapsed.is_none() {
                            collected.first_event_elapsed = Some(attempt_started.elapsed());
                        }
                        collected.content.push_str(&event.content);
                    }
                }
                Err(error) => {
                    collected.error = Some(error);
                    break;
                }
            }
        }
        if !saw_terminal_completion && collected.error.is_none() {
            // A closed channel is not equivalent to a provider's explicit
            // terminal event. Treat it as a failed (possibly partial) turn so
            // callers never persist truncated output as a successful answer.
            collected.error = Some(GatewayError::provider_failure(
                "provider stream closed before terminal completion",
            ));
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
            let (tx, rx) = mpsc::channel(2);
            tx.send(Ok(ChatEvent {
                content: format!("{}:{}", request.provider.as_str(), request.prompt),
                done: false,
            }))
            .await
            .expect("test receiver remains open");
            tx.send(Ok(ChatEvent {
                content: String::new(),
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
        let events = service
            .stream(BackendRequest {
                provider: ProviderId::new("claude").unwrap(),
                prompt: "hello".to_owned(),
                model: "test-model".to_owned(),
                has_history: false,
                thread_url: None,
            })
            .await
            .unwrap();
        let collected = service.collect(events, std::time::Instant::now()).await;
        assert_eq!(collected.content, "claude:hello");
        assert!(collected.error.is_none());
        assert!(collected.first_event_elapsed.is_some());
    }

    #[tokio::test]
    async fn collect_does_not_count_empty_chunks_as_first_content() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();

        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert!(collected.content.is_empty());
        assert!(
            collected.first_event_elapsed.is_none(),
            "empty heartbeat events must not be reported as TTFT"
        );
    }

    #[tokio::test]
    async fn collect_starts_ttft_at_first_non_empty_content_after_heartbeats() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(3);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: "answer".to_owned(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();

        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "answer");
        assert!(
            collected.first_event_elapsed.is_some(),
            "the first non-empty content event must establish TTFT"
        );
    }

    #[tokio::test]
    async fn collect_preserves_partial_content_but_reports_terminal_error() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(ChatEvent {
            content: "partial".into(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Err(GatewayError::provider_failure("upstream failed")))
            .await
            .unwrap();
        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "partial");
        assert_eq!(collected.error.unwrap().message, "upstream failed");
    }

    #[tokio::test]
    async fn collect_rejects_closed_stream_without_terminal_completion() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(ChatEvent {
            content: "partial answer".into(),
            done: false,
        }))
        .await
        .unwrap();
        drop(tx);

        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "partial answer");
        let error = collected.error.expect("missing terminal event must fail");
        assert!(error.message.contains("closed before terminal completion"));
    }

    #[tokio::test]
    async fn collect_treats_done_as_terminal_and_ignores_later_events() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(3);
        tx.send(Ok(ChatEvent {
            content: "answer".into(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: "must-not-append".into(),
            done: false,
        }))
        .await
        .unwrap();
        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "answer");
        assert!(collected.error.is_none());
    }
}

/// First meaningful event observed while starting a provider stream.
///
/// Empty non-terminal chunks are ignored; a terminal event without content is
/// distinct from a closed channel and from an upstream error so the router can
/// preserve its existing pre-output failover behavior.
#[derive(Debug)]
pub enum FirstEvent {
    Chunk(ChatEvent),
    EmptyCompletion,
    Error(GatewayError),
    Closed,
    Timeout,
}

pub(crate) fn usable_initial_stream_chunk(content: &str, done: bool) -> bool {
    !done && !content.is_empty()
}

/// Wait for the first meaningful event without allowing an empty stream to
/// stall provider failover indefinitely.
pub async fn first_event(
    rx: &mut mpsc::Receiver<Result<ChatEvent, GatewayError>>,
    timeout: Duration,
) -> FirstEvent {
    match tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Some(Ok(chunk)) if usable_initial_stream_chunk(&chunk.content, chunk.done) => {
                    break Some(Ok(chunk));
                }
                Some(Ok(chunk)) if chunk.done => break Some(Ok(chunk)),
                Some(Ok(_empty_chunk)) => continue,
                Some(Err(error)) => break Some(Err(error)),
                None => break None,
            }
        }
    })
    .await
    {
        Ok(Some(Ok(chunk))) if usable_initial_stream_chunk(&chunk.content, chunk.done) => {
            FirstEvent::Chunk(chunk)
        }
        Ok(Some(Ok(_))) => FirstEvent::EmptyCompletion,
        Ok(Some(Err(error))) => FirstEvent::Error(error),
        Ok(None) => FirstEvent::Closed,
        Err(_) => FirstEvent::Timeout,
    }
}

#[cfg(test)]
mod first_event_tests {
    use super::{first_event, FirstEvent};
    use crate::chat_backend::{ChatEvent, GatewayError};
    use anyhow::Result;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn first_event_skips_empty_chunks_and_returns_first_content() {
        let (tx, mut rx) = mpsc::channel(3);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: "answer".into(),
            done: false,
        }))
        .await
        .unwrap();
        let event = first_event(&mut rx, Duration::from_secs(1)).await;
        assert!(matches!(event, FirstEvent::Chunk(chunk) if chunk.content == "answer"));
    }

    #[tokio::test]
    async fn first_event_distinguishes_empty_completion_from_closed_stream() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();
        assert!(matches!(
            first_event(&mut rx, Duration::from_secs(1)).await,
            FirstEvent::EmptyCompletion
        ));
        drop(tx);
        assert!(matches!(
            first_event(&mut rx, Duration::from_secs(1)).await,
            FirstEvent::Closed
        ));
    }

    #[tokio::test]
    async fn first_event_preserves_backend_error() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Err(GatewayError::provider_failure("upstream failed")))
            .await
            .unwrap();
        assert!(
            matches!(first_event(&mut rx, Duration::from_secs(1)).await, FirstEvent::Error(error) if error.message == "upstream failed")
        );
    }

    #[tokio::test]
    async fn first_event_times_out_when_provider_never_emits() {
        let (_tx, mut rx) = mpsc::channel::<Result<ChatEvent, GatewayError>>(1);
        assert!(matches!(
            first_event(&mut rx, Duration::from_millis(1)).await,
            FirstEvent::Timeout
        ));
    }
}
