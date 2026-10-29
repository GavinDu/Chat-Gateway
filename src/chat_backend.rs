//! Narrow backend seam for canonical chat orchestration.
//!
//! The HTTP layer owns request validation, context construction, provider routing,
//! and response framing. A backend only executes a prepared provider request and
//! returns a stream of provider chunks. This lets tests substitute a deterministic
//! fake without constructing the thread-affine browser engine.
use crate::providers::ProviderChunk;
use anyhow::Result;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
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
    ) -> Result<mpsc::Receiver<Result<ProviderChunk>>>;
}

#[async_trait::async_trait]
impl ChatBackend for crate::browser::pool::ObscuraPool {
    async fn send_stream(
        &self,
        request: BackendRequest,
    ) -> Result<mpsc::Receiver<Result<ProviderChunk>>> {
        crate::browser::pool::ObscuraPool::send_stream(
            self,
            &request.provider,
            request.prompt,
            request.model,
            request.has_history,
            request.thread_url,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{BackendRequest, ChatBackend};
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
        ) -> Result<mpsc::Receiver<Result<ProviderChunk>>> {
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
