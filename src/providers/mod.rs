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
        ADAPTERS
            .iter()
            .find(|spec| spec.provider == *self)
            .map(|spec| spec.id)
            .expect("every Provider variant must have an adapter catalog row")
    }

    pub fn from_name(name: &str) -> Option<Provider> {
        ADAPTERS
            .iter()
            .find(|spec| spec.id == name)
            .map(|spec| spec.provider)
    }

    pub fn login_url(&self) -> &'static str {
        ADAPTERS
            .iter()
            .find(|spec| spec.provider == *self)
            .map(|spec| spec.url)
            .expect("every Provider variant must have an adapter catalog row")
    }

    pub fn all() -> Vec<Provider> {
        ADAPTERS.iter().map(|spec| spec.provider).collect()
    }
}

/// Provider adapter contract. `?Send` because adapters run on the
/// browser thread (see browser::pool) where obscura pages live.
#[async_trait(?Send)]
pub trait SiteAdapter: Send + Sync {
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

    /// Fetch text, node count, and streaming state in one page evaluation.
    fn response_probe(&self, page: &Page, check_body_challenge: bool) -> common::ResponseProbe {
        common::response_probe(page, self.selector_set(), check_body_challenge)
    }

    /// Completion strategy selected by this provider's adapter catalog row.
    fn done_signal(&self) -> DoneSignal {
        ADAPTERS
            .iter()
            .find(|spec| spec.id == self.name())
            .map(|spec| spec.done)
            .unwrap_or(DoneSignal::StableResponseText)
    }

    /// Best-effort cancellation when the HTTP consumer disconnects.
    /// Providers with a Stop control use the shared selector; adapters may
    /// override this method when their UI needs a different action.
    fn cancel_generation(&self, page: &Page) {
        let _ = common::click_first(page, &[r#"button[aria-label*="Stop"]"#]);
    }

    fn selectors(&self) -> AdapterSelectors;

    /// Full selector data (for --test-adapter probing over the whole
    /// fallback arrays, not just the primaries).
    fn selector_set(&self) -> &common::SelectorSet;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSignal {
    SharedStopControl,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneSignal {
    /// Require the conservative idle grace after response text stabilizes.
    StableResponseText,
    /// If a generation indicator was observed, a stable disappearance is an
    /// explicit completion signal; otherwise use the conservative fallback.
    StopControlDisappeared,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ThreadSupport {
    UrlAddressable,
    SidebarClick,
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingStrategy {
    SharedComposer,
}

type AdapterFactory = fn(ProviderConfig) -> Arc<dyn SiteAdapter>;

#[allow(dead_code)]
pub struct AdapterSpec {
    pub provider: Provider,
    pub id: &'static str,
    pub url: &'static str,
    pub selectors: &'static common::SelectorSet,
    pub model_pairs: &'static [(&'static str, &'static str)],
    pub strip_citations: bool,
    pub streaming: StreamSignal,
    pub done: DoneSignal,
    pub threads: ThreadSupport,
    pub typing: TypingStrategy,
    factory: AdapterFactory,
}

const fn adapter_spec(
    provider: Provider,
    id: &'static str,
    selectors: &'static common::SelectorSet,
    strip_citations: bool,
    done: DoneSignal,
    threads: ThreadSupport,
    factory: AdapterFactory,
) -> AdapterSpec {
    AdapterSpec {
        provider,
        id,
        url: selectors.url,
        selectors,
        model_pairs: selectors.model_pairs,
        strip_citations,
        streaming: StreamSignal::SharedStopControl,
        done,
        threads,
        typing: TypingStrategy::SharedComposer,
        factory,
    }
}

pub const ADAPTERS: &[AdapterSpec] = &[
    adapter_spec(
        Provider::Qwen,
        "qwen",
        &qwen::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        ThreadSupport::UrlAddressable,
        |cfg| Arc::new(qwen::QwenAdapter { cfg }),
    ),
    adapter_spec(
        Provider::MetaAI,
        "metaai",
        &metaai::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        ThreadSupport::SidebarClick,
        |cfg| Arc::new(metaai::MetaAIAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Claude,
        "claude",
        &claude::SELECTORS,
        false,
        DoneSignal::StopControlDisappeared,
        ThreadSupport::UrlAddressable,
        |cfg| Arc::new(claude::ClaudeAdapter { cfg }),
    ),
    adapter_spec(
        Provider::ChatGPT,
        "chatgpt",
        &chatgpt::SELECTORS,
        false,
        DoneSignal::StopControlDisappeared,
        ThreadSupport::UrlAddressable,
        |cfg| Arc::new(chatgpt::ChatGPTAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Gemini,
        "gemini",
        &gemini::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        ThreadSupport::UrlAddressable,
        |cfg| Arc::new(gemini::GeminiAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Grok,
        "grok",
        &grok::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        ThreadSupport::UrlAddressable,
        |cfg| Arc::new(grok::GrokAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Mistral,
        "mistral",
        &mistral::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        ThreadSupport::UrlAddressable,
        |cfg| Arc::new(mistral::MistralAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Perplexity,
        "perplexity",
        &perplexity::SELECTORS,
        true,
        DoneSignal::StableResponseText,
        ThreadSupport::UrlAddressable,
        |cfg| Arc::new(perplexity::PerplexityAdapter { cfg }),
    ),
];

fn adapter_for(provider: Provider, cfg: ProviderConfig) -> Arc<dyn SiteAdapter> {
    let spec = ADAPTERS
        .iter()
        .find(|spec| spec.provider == provider)
        .expect("every Provider variant must have an adapter catalog row");
    (spec.factory)(cfg)
}

/// Registry of adapters for the enabled, known providers in the config.
pub fn build_registry(cfg: &AppConfig) -> HashMap<Provider, Arc<dyn SiteAdapter>> {
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

/// A response-node count remains above the baseline for every subsequent poll.
/// Reset the text tracker only on the first observation of a new node.
fn should_reset_response_tracker(
    already_detected: bool,
    baseline_count: usize,
    current_count: usize,
) -> bool {
    !already_detected && is_new_response_node(baseline_count, current_count)
}

/// Avoid evaluating the DOM for response-node counts after the new response
/// node has already been detected; the count remains elevated on every poll.
fn should_probe_response_count(response_node_detected: bool) -> bool {
    !response_node_detected
}

/// Challenge-page body extraction is slower than URL inspection; sample it every five polls.
fn should_check_body_challenge(poll_count: u32) -> bool {
    poll_count.is_multiple_of(5)
}

/// Generic send + stream-poll loop shared by all adapters. Poll response
/// text every 350ms and finish only after a conservative idle grace period
/// with no streaming indicator, or at the provider timeout. The grace period
/// tolerates long reasoning/search pauses that can occur between text deltas.
/// Runs on the browser thread; chunks leave via the channel.
const COMPLETION_IDLE_TICKS: u32 = 43; // ~15 seconds at 350ms per poll.
const EXPLICIT_DONE_IDLE_TICKS: u32 = 3; // Confirm stop-control disappearance for ~1 second.

fn completion_idle_ticks(
    done_signal: DoneSignal,
    saw_streaming: bool,
    post_answer_affordance: bool,
) -> u32 {
    match done_signal {
        DoneSignal::StableResponseText => COMPLETION_IDLE_TICKS,
        DoneSignal::StopControlDisappeared if saw_streaming && post_answer_affordance => {
            EXPLICIT_DONE_IDLE_TICKS
        }
        // Never infer completion from disappearance alone: require a known
        // post-answer affordance, otherwise retain the conservative fallback.
        DoneSignal::StopControlDisappeared => COMPLETION_IDLE_TICKS,
    }
}

fn completion_ready(
    stable_ticks: u32,
    streaming: bool,
    done_signal: DoneSignal,
    saw_streaming: bool,
    post_answer_affordance: bool,
) -> bool {
    stable_ticks >= completion_idle_ticks(done_signal, saw_streaming, post_answer_affordance)
        && !streaming
}

pub async fn run_send(
    adapter: Arc<dyn SiteAdapter>,
    page: Rc<Page>,
    prompt: String,
    timeout: Duration,
    chunks: mpsc::Sender<Result<ProviderChunk>>,
) {
    if crate::provider_guard::is_challenge_page(&page) {
        crate::provider_guard::disable_provider(
            adapter.name(),
            "challenge page detected; manual review required",
        );
        let _ = chunks
            .send(Err(anyhow::anyhow!(
                "provider '{}' disabled after challenge page detection; manual review required",
                adapter.name()
            )))
            .await;
        return;
    }

    // Baseline snapshot (Go concept, Rust form): capture the response
    // text BEFORE sending, so pre-existing text on a reused warm page
    // is never emitted as new. Rust v1 had no baseline (last_len
    // started at 0); this fixes that stale-text bug while keeping
    // Rust v1's 350ms / stability / indicator completion.
    let done_signal = adapter.done_signal();
    let baseline_probe = adapter.response_probe(&page, true);
    let baseline_response_count = baseline_probe.count;
    let baseline = baseline_probe.text.filter(|t| !t.is_empty());
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
    let mut last_delta_at: Option<Instant> = None;
    let mut saw_streaming = false;
    let mut response_node_detected = false;
    let mut poll_count = 0u32;

    loop {
        if chunks.is_closed() {
            adapter.cancel_generation(page.as_ref());
            tracing::debug!(
                provider = adapter.name(),
                "client disconnected; requested generation stop"
            );
            return;
        }

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
        let poll_started = Instant::now();
        poll_count = poll_count.saturating_add(1);
        // Body text extraction is expensive on large pages; keep URL checks on every
        // tick, but inspect challenge text every five polls (~1.75 seconds).
        let check_body_challenge = should_check_body_challenge(poll_count);
        let probe = adapter.response_probe(&page, check_body_challenge);
        crate::metrics::record_browser_poll(
            adapter.name(),
            1,
            poll_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        );
        let lower_url = probe.url.to_ascii_lowercase();
        let url_challenge = [
            "captcha",
            "challenge",
            "checkpoint",
            "verify-human",
            "verify_you_are_human",
        ]
        .iter()
        .any(|marker| lower_url.contains(marker));

        if probe.challenge || url_challenge {
            crate::provider_guard::disable_provider(
                adapter.name(),
                "challenge page detected; manual review required",
            );
            let _ = chunks
                .send(Err(anyhow::anyhow!(
                    "provider '{}' disabled after challenge page detection; manual review required",
                    adapter.name()
                )))
                .await;
            return;
        }
        if should_probe_response_count(response_node_detected)
            && should_reset_response_tracker(
                response_node_detected,
                baseline_response_count,
                probe.count,
            )
        {
            // A new DOM node is the strongest signal that the provider started
            // a fresh answer, even when its text matches the previous turn.
            // Latch this transition: the node remains present on later polls,
            // and resetting on every poll would re-emit the full answer.
            response_node_detected = true;
            tracker = ResponseDeltaTracker::new(None);
        }

        let streaming = probe.streaming;
        saw_streaming |= streaming;

        if let Some(txt) = probe.text {
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
                    if stable_ticks >= completion_idle_ticks(done_signal, saw_streaming, probe.post_answer_affordance) {
                        if let Some(answer) = &identical_answer_fallback {
                            let answer = if let Some(re) = &citation_re {
                                re.replace_all(answer, "").to_string()
                            } else {
                                answer.clone()
                            };
                            if chunks
                                .send(Ok(ProviderChunk {
                                    content: answer,
                                    done: false,
                                }))
                                .await
                                .is_err()
                            {
                                adapter.cancel_generation(page.as_ref());
                                return;
                            }
                        }
                        if let Some(last_delta_at) = last_delta_at {
                            crate::metrics::record_provider_tail(
                                adapter.name(),
                                last_delta_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                            );
                        }
                        let _ = chunks
                            .send(Ok(ProviderChunk {
                                content: String::new(),
                                done: true,
                            }))
                            .await;
                        break;
                    }
                    continue;
                }
                DeltaObservation::Delta(mut delta) => {
                    if let Some(re) = &citation_re {
                        delta = re.replace_all(&delta, "").to_string();
                    }
                    stable_ticks = 0;
                    if !delta.is_empty() {
                        last_delta_at = Some(Instant::now());
                    }
                    if !delta.is_empty()
                        && chunks
                            .send(Ok(ProviderChunk {
                                content: delta,
                                done: false,
                            }))
                            .await
                            .is_err()
                    {
                        adapter.cancel_generation(page.as_ref());
                        return; // receiver gone
                    }
                    continue;
                }
                DeltaObservation::Stable => {
                    if streaming {
                        stable_ticks = 0;
                    } else {
                        stable_ticks += 1;
                    }
                }
            }

            // Completion requires a conservative 15s idle grace plus no
            // active streaming indicator, avoiding truncation during long
            // reasoning or search pauses.
            if completion_ready(
                stable_ticks,
                streaming,
                done_signal,
                saw_streaming,
                probe.post_answer_affordance,
            ) {
                if let Some(last_delta_at) = last_delta_at {
                    crate::metrics::record_provider_tail(
                        adapter.name(),
                        last_delta_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    );
                }
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
    fn response_node_count_detects_new_node_but_not_reused_node() {
        assert!(!is_new_response_node(3, 3));
        assert!(is_new_response_node(3, 4));
        assert!(!is_new_response_node(4, 3));
    }

    #[test]
    fn response_count_probe_stops_after_new_node_is_detected() {
        assert!(should_probe_response_count(false));
        assert!(!should_probe_response_count(true));
    }

    #[test]
    fn challenge_body_scan_is_sampled_every_five_polls() {
        assert!(!should_check_body_challenge(1));
        assert!(!should_check_body_challenge(4));
        assert!(should_check_body_challenge(5));
        assert!(!should_check_body_challenge(6));
        assert!(should_check_body_challenge(10));
    }

    #[test]
    fn response_tracker_resets_only_once_after_new_node() {
        let mut tracker = ResponseDeltaTracker::new(Some("old answer".to_string()));
        if should_reset_response_tracker(false, 3, 4) {
            tracker = ResponseDeltaTracker::new(None);
        }
        assert_eq!(
            tracker.observe("same answer"),
            DeltaObservation::Delta("same answer".to_string())
        );

        // The DOM node remains present on later polls, but must not reset the
        // tracker again or cause the same complete answer to be emitted twice.
        assert!(!should_reset_response_tracker(true, 3, 4));
        assert_eq!(tracker.observe("same answer"), DeltaObservation::Stable);
        assert!(!should_reset_response_tracker(false, 3, 3));
    }

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
    use super::{
        completion_idle_ticks, completion_ready, DoneSignal, COMPLETION_IDLE_TICKS,
        EXPLICIT_DONE_IDLE_TICKS,
    };

    #[test]
    fn stable_text_requires_conservative_idle_grace() {
        assert!(!completion_ready(
            28,
            false,
            DoneSignal::StableResponseText,
            true,
        ));
        assert!(!completion_ready(
            COMPLETION_IDLE_TICKS - 1,
            false,
            DoneSignal::StableResponseText,
            true,
        ));
        assert!(completion_ready(
            COMPLETION_IDLE_TICKS,
            false,
            DoneSignal::StableResponseText,
            true,
        ));
    }

    #[test]
    fn active_streaming_prevents_completion_even_after_idle_ticks() {
        assert!(!completion_ready(
            COMPLETION_IDLE_TICKS + 10,
            true,
            DoneSignal::StableResponseText,
            true,
        ));
    }

    #[test]
    fn explicit_stop_disappearance_uses_short_grace_only_after_streaming_was_seen() {
        assert_eq!(
            completion_idle_ticks(DoneSignal::StopControlDisappeared, true),
            EXPLICIT_DONE_IDLE_TICKS,
        );
        assert_eq!(
            completion_idle_ticks(DoneSignal::StopControlDisappeared, false),
            COMPLETION_IDLE_TICKS,
        );
        assert!(!completion_ready(
            EXPLICIT_DONE_IDLE_TICKS - 1,
            false,
            DoneSignal::StopControlDisappeared,
            true,
        ));
        assert!(completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            false,
            DoneSignal::StopControlDisappeared,
            true,
        ));
        assert!(!completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            true,
            DoneSignal::StopControlDisappeared,
            true,
        ));
    }
}

#[cfg(test)]
mod provider_catalog_contract_tests {
    use super::{Provider, ADAPTERS};
    use std::collections::HashSet;

    #[test]
    fn provider_catalog_names_are_unique_and_round_trip_exactly() {
        let providers = Provider::all();
        assert_eq!(providers.len(), 8, "the public provider catalog changed");

        let mut names = HashSet::new();
        for provider in providers {
            let name = provider.name();
            assert!(names.insert(name), "duplicate provider name: {name}");
            assert_eq!(Provider::from_name(name), Some(provider));
            assert!(
                Provider::from_name(&name.to_ascii_uppercase()).is_none(),
                "provider lookup should remain exact and case-sensitive: {name}"
            );
            assert!(
                provider.login_url().starts_with("https://"),
                "provider login URL must be HTTPS: {name}"
            );
        }
    }

    #[test]
    fn adapter_table_is_the_single_unique_provider_catalog() {
        assert_eq!(ADAPTERS.len(), 8);
        let mut ids = HashSet::new();
        let mut urls = HashSet::new();
        for spec in ADAPTERS {
            assert!(ids.insert(spec.id), "duplicate provider id: {}", spec.id);
            assert!(
                urls.insert(spec.url),
                "duplicate provider URL: {}",
                spec.url
            );
            assert_eq!(spec.provider.name(), spec.id);
            assert_eq!(spec.provider.login_url(), spec.url);
            assert_eq!(spec.selectors.url, spec.url);
            assert_eq!(spec.model_pairs, spec.selectors.model_pairs);
            let keywords: Vec<_> = spec.selectors.model_keywords.split('|').collect();
            let unique: HashSet<_> = keywords.iter().copied().collect();
            assert_eq!(
                keywords.len(),
                unique.len(),
                "duplicate model keyword for {}",
                spec.id
            );
            assert!(spec.url.starts_with("https://"));
        }
    }

    #[test]
    fn unknown_provider_names_do_not_silently_select_a_provider() {
        for name in ["", "unknown", "openai", "claude ", " Qwen"] {
            assert_eq!(
                Provider::from_name(name),
                None,
                "unexpected match for {name:?}"
            );
        }
    }
}
