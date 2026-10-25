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

    /// Count distinct response nodes before/after a send. This lets the
    /// stream loop distinguish a new identical answer from stale text.
    fn response_count(&self, page: &Page) -> usize {
        let selectors = serde_json::to_string(self.selector_set().response)
            .unwrap_or_else(|_| "[]".to_owned());
        page.evaluate(&format!(
            "(() => {{ const nodes = new Set(); for (const selector of {selectors}) {{ try {{ document.querySelectorAll(selector).forEach(node => nodes.add(node)); }} catch (_) {{}} }} return nodes.size; }})()"
        ))
        .as_u64()
        .unwrap_or(0) as usize
    }

    /// Minimum quiet period before declaring a response complete. The default
    /// tolerates long pauses during reasoning, search, and tool use.
    fn response_idle_grace(&self) -> Duration {
        Duration::from_secs(10)
    }

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

fn is_new_response_node(baseline_count: usize, current_count: usize) -> bool {
    current_count > baseline_count
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
    let baseline_response_count = adapter.response_count(&page);
    let baseline = adapter.response_text(&page).filter(|t| !t.is_empty());
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
    let mut new_response_node_seen = false;
    let mut stable_since: Option<Instant> = None;
    let idle_grace = adapter.response_idle_grace();

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

        if !new_response_node_seen
            && is_new_response_node(baseline_response_count, adapter.response_count(&page))
        {
            // A new DOM node is the strongest signal that the provider started
            // a fresh answer, even when its text exactly matches the last turn.
            tracker = ResponseDeltaTracker::new(None);
            new_response_node_seen = true;
        }

        if let Some(txt) = adapter.response_text(&page) {
            match tracker.observe(&txt) {
                DeltaObservation::NoText
                | DeltaObservation::WaitingForNewResponse
                | DeltaObservation::InvalidBoundary => continue,
                DeltaObservation::Delta(mut delta) => {
                    if let Some(re) = &citation_re {
                        delta = re.replace_all(&delta, "").to_string();
                    }
                    stable_since = None;
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
                DeltaObservation::Stable => {
                    stable_since.get_or_insert_with(Instant::now);
                }
            }

            // Require a longer quiet window so reasoning/search/tool pauses
            // do not prematurely terminate a response.
            if stable_since.is_some_and(|since| since.elapsed() >= idle_grace)
                && !adapter.is_streaming(&page)
            {
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
mod response_completion_regression_tests {
    use super::*;

    #[test]
    fn new_response_node_resets_tracker_for_identical_consecutive_answers() {
        let baseline_count = 2;
        let current_count = 3;
        assert!(is_new_response_node(baseline_count, current_count));

        let mut tracker = ResponseDeltaTracker::new(Some("OK".to_owned()));
        assert_eq!(
            tracker.observe("OK"),
            DeltaObservation::WaitingForNewResponse
        );

        // run_send resets the tracker when the node count grows. The identical
        // answer is therefore emitted instead of being mistaken for stale text.
        tracker = ResponseDeltaTracker::new(None);
        assert_eq!(tracker.observe("OK"), DeltaObservation::Delta("OK".to_owned()));
    }

    #[test]
    fn unchanged_response_node_is_not_mistaken_for_a_new_answer() {
        assert!(!is_new_response_node(2, 2));
        assert!(!is_new_response_node(2, 1));
    }
}
