pub mod common;
pub mod qwen;
pub mod metaai;
pub mod claude;
pub mod chatgpt;
pub mod gemini;
pub mod grok;
pub mod mistral;
pub mod perplexity;

use async_trait::async_trait;
use obscura_core::Page;
use anyhow::Result;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use crate::config::{AppConfig, ProviderConfig};

/// One streamed piece of a provider response.
#[derive(Debug, Clone)]
pub struct ProviderChunk {
    pub content: String,
    pub done: bool,
}

/// Primary selectors for an adapter (diagnostics + readiness reporting).
#[derive(Debug, Clone)]
pub struct AdapterSelectors {
    pub input_selector: String,
    pub send_button: String,
    pub response_container: String,
    pub model_picker: String,
}

/// The 8 supported providers (v2's enum, kept in providers:: where the
/// adapters live; browser::pool re-exports it).
#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug)]
pub enum Provider {
    Qwen,
    MetaAI,
    Claude,
    ChatGPT,
    Gemini,
    Grok,
    Mistral,
    Perplexity,
}

impl Provider {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Qwen => "qwen",
            Self::MetaAI => "metaai",
            Self::Claude => "claude",
            Self::ChatGPT => "chatgpt",
            Self::Gemini => "gemini",
            Self::Grok => "grok",
            Self::Mistral => "mistral",
            Self::Perplexity => "perplexity",
        }
    }

    pub fn from_name(name: &str) -> Option<Provider> {
        Self::all().into_iter().find(|p| p.name() == name)
    }

    pub fn login_url(&self) -> &'static str {
        match self {
            Self::Qwen => qwen::SELECTORS.url,
            Self::MetaAI => metaai::SELECTORS.url,
            Self::Claude => claude::SELECTORS.url,
            Self::ChatGPT => chatgpt::SELECTORS.url,
            Self::Gemini => gemini::SELECTORS.url,
            Self::Grok => grok::SELECTORS.url,
            Self::Mistral => mistral::SELECTORS.url,
            Self::Perplexity => perplexity::SELECTORS.url,
        }
    }

    pub fn all() -> Vec<Provider> {
        vec![Self::Qwen, Self::MetaAI, Self::Claude, Self::ChatGPT, Self::Gemini, Self::Grok, Self::Mistral, Self::Perplexity]
    }
}

/// Provider adapter contract. `?Send` because adapters run on the
/// browser thread (see browser::pool) where obscura pages live.
#[async_trait(?Send)]
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;
    fn aliases(&self) -> &[String];
    /// Perplexity answers carry [1][2] citation markers; strip them.
    fn strip_citations(&self) -> bool { false }

    /// Discover models without sending a message - reads the picker.
    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>>;

    /// Model-picker trigger→option pairs (Go's modelSelectors data).
    /// Default: none — selection no-ops, which is Rust v1 behavior.
    fn model_pairs(&self) -> &[(&str, &str)] { &[] }

    /// Select a model in the provider UI before typing. Default: no-op
    /// (Rust v1 behavior — no selection). Providers with pair data
    /// delegate to common::select_model; failure fails the attempt so
    /// the router can try the next provider.
    async fn select_model(&self, _page: &Page, _model: &str) -> Result<()> { Ok(()) }

    /// Wait for the input, type the prompt, click send.
    async fn send_prompt(&self, page: &Page, prompt: &str) -> Result<()>;

    /// Current full text of the (last) response, if present.
    fn response_text(&self, page: &Page) -> Option<String>;

    /// Provider still generating (stop button / streaming attribute).
    fn is_streaming(&self, page: &Page) -> bool {
        common::is_streaming(page)
    }

    fn selectors(&self) -> AdapterSelectors;

    /// Full selector data (for --test-adapter probing over the whole
    /// fallback arrays, not just the primaries).
    fn selector_set(&self) -> &common::SelectorSet;
}

fn adapter_for(provider: Provider, cfg: ProviderConfig) -> Arc<dyn ChatProvider> {
    match provider {
        Provider::Qwen => Arc::new(qwen::QwenAdapter { cfg }),
        Provider::MetaAI => Arc::new(metaai::MetaAIAdapter { cfg }),
        Provider::Claude => Arc::new(claude::ClaudeAdapter { cfg }),
        Provider::ChatGPT => Arc::new(chatgpt::ChatGPTAdapter { cfg }),
        Provider::Gemini => Arc::new(gemini::GeminiAdapter { cfg }),
        Provider::Grok => Arc::new(grok::GrokAdapter { cfg }),
        Provider::Mistral => Arc::new(mistral::MistralAdapter { cfg }),
        Provider::Perplexity => Arc::new(perplexity::PerplexityAdapter { cfg }),
    }
}

/// Registry of adapters for the enabled, known providers in the config.
pub fn build_registry(cfg: &AppConfig) -> HashMap<Provider, Arc<dyn ChatProvider>> {
    let mut map = HashMap::new();
    for p in &cfg.providers {
        if !p.enabled { continue; }
        if let Some(provider) = Provider::from_name(&p.name) {
            map.insert(provider, adapter_for(provider, p.clone()));
        }
    }
    map
}

/// Generic send + stream-poll loop (v1 semantics, shared by all
/// adapters): type the prompt via the adapter, then poll the response
/// container every 350ms, yield text deltas, finish after ~2s without
/// growth and no streaming indicator, or at the provider timeout.
/// Runs on the browser thread; chunks leave via the channel.
pub async fn run_send(
    adapter: Arc<dyn ChatProvider>,
    page: Rc<Page>,
    prompt: String,
    timeout: Duration,
    chunks: mpsc::Sender<Result<ProviderChunk>>,
) {
    // Baseline snapshot (Go concept, Rust form): capture the response
    // text BEFORE sending, so pre-existing text on a reused warm page
    // is never emitted as new. Rust v1 had no baseline (last_len
    // started at 0); this fixes that stale-text bug while keeping
    // Rust v1's 350ms / stability / indicator completion.
    let baseline = adapter.response_text(&page).filter(|t| !t.is_empty());
    let mut baseline = baseline;

    if let Err(e) = adapter.send_prompt(&page, &prompt).await {
        let _ = chunks.send(Err(e)).await;
        return;
    }

    let citation_re = adapter
        .strip_citations()
        .then(|| regex::Regex::new(r"\[\d+\]").expect("static regex"));
    let mut last_len = 0usize;
    let mut stable_ticks = 0u32;
    let start = Instant::now();

    loop {
        if start.elapsed() > timeout {
            // A timeout is not a successful completion. Propagate a typed
            // provider error so the API won't commit a partial/empty turn.
            let _ = chunks.send(Err(anyhow::anyhow!(
                "provider '{}' did not complete within {:?}",
                adapter.name(),
                timeout
            ))).await;
            break;
        }

        tokio::time::sleep(Duration::from_millis(350)).await;

        if let Some(txt) = adapter.response_text(&page) {
            if txt.is_empty() {
                continue;
            }
            // Still at baseline: the provider has not produced new
            // text yet. Do not emit, do not count stability.
            if let Some(b) = &baseline {
                if txt == *b {
                    continue;
                }
                // First differing text: emit only the growth past the
                // baseline (suffix if it extends the baseline, else
                // the full text), then drop the baseline.
                let mut delta = if let Some(rest) = txt.strip_prefix(b.as_str()) {
                    rest.to_string()
                } else {
                    txt.clone()
                };
                if let Some(re) = &citation_re {
                    delta = re.replace_all(&delta, "").to_string();
                }
                last_len = txt.len();
                stable_ticks = 0;
                baseline = None;
                if !delta.is_empty()
                    && chunks.send(Ok(ProviderChunk { content: delta, done: false })).await.is_err()
                {
                    return; // receiver gone
                }
                continue;
            }
            if txt.len() > last_len {
                // v1 sliced bytes directly; guard the char boundary so a
                // multi-byte char mid-growth cannot panic the poll loop.
                if !txt.is_char_boundary(last_len) {
                    continue;
                }
                let mut delta = txt[last_len..].to_string();
                if let Some(re) = &citation_re {
                    delta = re.replace_all(&delta, "").to_string();
                }
                last_len = txt.len();
                stable_ticks = 0;
                if !delta.is_empty()
                    && chunks.send(Ok(ProviderChunk { content: delta, done: false })).await.is_err()
                {
                    return; // receiver gone
                }
            } else {
                stable_ticks += 1;
            }

            // Completion: no growth for ~2s + no streaming indicator
            if stable_ticks > 6 && !adapter.is_streaming(&page) {
                let _ = chunks.send(Ok(ProviderChunk { content: String::new(), done: true })).await;
                break;
            }
        }
    }

    tracing::info!(provider = adapter.name(), total_ms = %start.elapsed().as_millis(), "generation complete");
}
