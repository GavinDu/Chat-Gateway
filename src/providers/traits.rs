use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub conversation_id: Option<String>,
    pub messages: Vec<crate::memory::ChatMessage>,
    pub model: String,
    pub stream: bool,
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderChunk {
    pub content: String,
    pub done: bool,
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = anyhow::Result<ProviderChunk>> + Send>>;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;
    fn aliases(&self) -> &[String];
    fn priority(&self) -> u32 { 0 }

    // Discover models without sending a message - reads model/bot picker
    async fn discover_models(&self, page: &chromiumoxide::Page) -> anyhow::Result<Vec<String>>;

    // Send message via Obscura page
    async fn send(&self, page: &chromiumoxide::Page, req: ProviderRequest) -> anyhow::Result<ChunkStream>;

    fn selectors(&self) -> AdapterSelectors;
}

#[derive(Debug, Clone)]
pub struct AdapterSelectors {
    pub input_selector: String,
    pub send_button: String,
    pub response_container: String,
    pub model_picker: String,
}
