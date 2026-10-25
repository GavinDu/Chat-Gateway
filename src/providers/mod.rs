pub mod chatgpt;
pub mod claude;
pub mod common;
pub mod gemini;
pub mod grok;
pub mod metaai;
pub mod mistral;
pub mod perplexity;
pub mod qwen;

use crate::config::{AppConfig, ProviderConfig};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// One streamed piece of a provider response.
#[derive(Debug, Clone)]
pub struct ProviderChunk {
    pub content: String,
    pub done: bool,
}

/// Primary selectors for an adapter (diagnostics + readiness reporting).
// Selector metadata is retained for upcoming adapter contract/readiness diagnostics.
#[allow(dead_code)]
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
        vec![
            Self::Qwen,
            Self::MetaAI,
            Self::Claude,
            Self::ChatGPT,
            Self::Gemini,
            Self::Grok,
            Self::Mistral,
            Self::Perplexity,
        ]
    }
}

/// Provider adapter contract. `?Send` because adapters run on the
/// browser thread (see browser::pool) where obscura pages live.
#[async_trait(?Send)]
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;
    // Kept in the adapter contract for provider catalog diagnostics.
    #[allow(dead_code)]
    fn aliases(&self) -> &[String];
    /// Perplexity answers carry [1][2] citation markers; strip them.
    fn strip_citations(&self) -> bool {
        false
    }

    /// Discover models without sending a message - reads the picker.
    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>>;

    /// Model-picker trigger→option pairs (Go's modelSelectors data).
    /// Default: none — selection no-ops, which is Rust v1 behavior.
    // Picker metadata is consumed by providers that implement UI model selection.
    #[allow(dead_code)]
    fn model_pairs(&self) -> &[(&str, &str)] {
        &[]
    }

    /// Select a model in the provider UI before typing. Default: no-op
    /// (Rust v1 behavior — no selection). Providers with pair data
    /// delegate to common::select_model; failure fails the attempt so
    /// the router can try the next provider.
    async fn select_model(&self, _page: &Page, _model: &str) -> Result<()> {
        Ok(())
    }

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
        if !p.enabled {
            continue;
        }
        if let Some(provider) = Provider::from_name(&p.name) {
            map.insert(provider, adapter_for(provider, p.clone()));
        }
    }
    map
}

#[derive(Debug, PartialEq, Eq)]
enum DeltaObservation {
    NoText,
    WaitingForNewResponse,
    InvalidBoundary,
    Delta(String),
    Stable,
}

/// Tracks byte offsets without ever slicing a UTF-8 string at an invalid
/// boundary. Baseline text is from the previous answer on a reused page.
struct ResponseDeltaTracker {
    baseline: Option<String>,
    last_len: usize,
}

impl ResponseDeltaTracker {
    fn new(baseline: Option<String>) -> Self {
        Self {
            baseline,
            last_len: 0,
        }
    }

    fn observe(&mut self, text: &str) -> DeltaObservation {
        if text.is_empty() {
            return DeltaObservation::NoText;
        }
        if let Some(baseline) = &self.baseline {
            if text == baseline {
                return DeltaObservation::WaitingForNewResponse;
            }
            let delta = text
                .strip_prefix(baseline.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| text.to_string());
            self.last_len = text.len();
            self.baseline = None;
            return DeltaObservation::Delta(delta);
        }
        if text.len() > self.last_len {
            if !text.is_char_boundary(self.last_len) {
                return DeltaObservation::InvalidBoundary;
            }
            let delta = text[self.last_len..].to_string();
            self.last_len = text.len();
            return DeltaObservation::Delta(delta);
        }
        DeltaObservation::Stable
    }
}

/// Generic send + stream-poll loop shared by all adapters. Poll response
/// text every 350ms and finish only after a conservative idle grace period
/// with no streaming indicator, or at the provider timeout. The grace period
/// tolerates long reasoning/search pauses that can occur between text deltas.
/// Runs on the browser thread; chunks leave via the channel.
const COMPLETION_IDLE_TICKS: u32 = 43; // ~15 seconds at 350ms per poll.

fn completion_ready(stable_ticks: u32, streaming: bool) -> bool {
    stable_ticks >= COMPLETION_IDLE_TICKS && !streaming
}

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
    let identical_answer_fallback = baseline.clone();
    let mut baseline = baseline;

    // Apply the same timeout budget to prompt submission and response
    // polling; otherwise a stuck selector/input action can hang indefinitely
    // before the existing polling timeout even starts.
    let start = Instant::now();
    match tokio::time::timeout(timeout, adapter.send_prompt(&page, &prompt)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = chunks.send(Err(error)).await;
            return;
        }
        Err(_) => {
            let _ = chunks
                .send(Err(anyhow::anyhow!(
                    "provider '{}' timed out while submitting the prompt after {:?}",
                    adapter.name(),
                    timeout
                )))
                .await;
            return;
        }
    }

    let citation_re = adapter
        .strip_citations()
        .then(|| regex::Regex::new(r"\[\d+\]").expect("static regex"));
    let mut tracker = ResponseDeltaTracker::new(baseline.take());
    let mut stable_ticks = 0u32;
    let mut saw_streaming = false;

    loop {
        if start.elapsed() > timeout {
            // A timeout is not a successful completion. Propagate a typed
            // provider error so the API won't commit a partial/empty turn.
            let _ = chunks
                .send(Err(anyhow::anyhow!(
                    "provider '{}' did not complete within {:?}",
                    adapter.name(),
                    timeout
                )))
                .await;
            break;
        }

        tokio::time::sleep(Duration::from_millis(350)).await;

        let streaming = adapter.is_streaming(&page);
        saw_streaming |= streaming;

        if let Some(txt) = adapter.response_text(&page) {
            match tracker.observe(&txt) {
                DeltaObservation::NoText | DeltaObservation::InvalidBoundary => continue,
                DeltaObservation::WaitingForNewResponse => {
                    // Some providers reuse a response node and can return the
                    // exact same text on consecutive turns. Once a generation
                    // indicator has been observed, allow a full idle grace for
                    // that indicator to disappear before treating the unchanged
                    // baseline as the new answer.
                    if saw_streaming && !streaming {
                        stable_ticks += 1;
                    } else {
                        stable_ticks = 0;
                        continue;
                    }
                    if stable_ticks >= COMPLETION_IDLE_TICKS {
                        if let Some(answer) = &identical_answer_fallback {
                            if chunks
                                .send(Ok(ProviderChunk {
                                    content: answer.clone(),
                                    done: false,
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        let _ = chunks.send(Ok(ProviderChunk {
                            content: String::new(),
                            done: true,
                        })).await;
                        break;
                    }
                    continue;
                },
                DeltaObservation::Delta(mut delta) => {
                    if let Some(re) = &citation_re {
                        delta = re.replace_all(&delta, "").to_string();
                    }
                    stable_ticks = 0;
                    if !delta.is_empty()
                        && chunks
                            .send(Ok(ProviderChunk {
                                content: delta,
                                done: false,
                            }))
                            .await
                            .is_err()
                    {
                        return; // receiver gone
                    }
                    continue;
                }
                DeltaObservation::Stable => stable_ticks += 1,
            }

            // Completion requires a conservative 15s idle grace plus no
            // active streaming indicator, avoiding truncation during long
            // reasoning or search pauses.
            if completion_ready(stable_ticks, streaming) {
                let _ = chunks
                    .send(Ok(ProviderChunk {
                        content: String::new(),
                        done: true,
                    }))
                    .await;
                break;
            }
        }
    }

    tracing::info!(provider = adapter.name(), total_ms = %start.elapsed().as_millis(), "generation complete");
}

#[cfg(test)]
mod response_delta_tracker_tests {
    use super::*;

    #[test]
    fn waits_when_reused_page_still_shows_old_answer() {
        let mut tracker = ResponseDeltaTracker::new(Some("same answer".to_string()));
        assert_eq!(
            tracker.observe("same answer"),
            DeltaObservation::WaitingForNewResponse
        );
        assert_eq!(
            tracker.observe("same answer"),
            DeltaObservation::WaitingForNewResponse
        );
        assert_eq!(
            tracker.observe("same answer plus new text"),
            DeltaObservation::Delta(" plus new text".to_string())
        );
    }

    #[test]
    fn emits_unicode_growth_without_splitting_utf8() {
        let mut tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("你好"),
            DeltaObservation::Delta("你好".to_string())
        );
        assert_eq!(
            tracker.observe("你好世界"),
            DeltaObservation::Delta("世界".to_string())
        );
        assert_eq!(tracker.observe("你好世界"), DeltaObservation::Stable);
    }

    #[test]
    fn emits_full_text_when_provider_replaces_instead_of_appending() {
        let mut tracker = ResponseDeltaTracker::new(Some("old answer".to_string()));
        assert_eq!(
            tracker.observe("new answer"),
            DeltaObservation::Delta("new answer".to_string())
        );
    }

    #[test]
    fn ignores_empty_text_until_response_appears() {
        let mut tracker = ResponseDeltaTracker::new(None);
        assert_eq!(tracker.observe(""), DeltaObservation::NoText);
        assert_eq!(
            tracker.observe("answer"),
            DeltaObservation::Delta("answer".to_string())
        );
    }
}


#[cfg(test)]
mod response_completion_tests {
    use super::{completion_ready, COMPLETION_IDLE_TICKS};

    #[test]
    fn requires_more_than_ten_seconds_of_idle_before_completion() {
        assert!(!completion_ready(28, false));
        assert!(!completion_ready(COMPLETION_IDLE_TICKS - 1, false));
        assert!(completion_ready(COMPLETION_IDLE_TICKS, false));
    }

    #[test]
    fn active_streaming_prevents_completion_even_after_idle_ticks() {
        assert!(!completion_ready(COMPLETION_IDLE_TICKS + 10, true));
    }
}
