pub mod chatgpt;
pub mod claude;
pub mod common;
pub mod gemini;
pub mod grok;
pub mod manus;
pub mod metaai;
pub mod mistral;
pub mod perplexity;
pub mod qwen;

use crate::config::{AppConfig, ProviderConfig};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// One streamed piece of a provider response.
#[derive(Debug, Clone)]
pub struct ProviderChunk {
    pub content: String,
    pub done: bool,
}

/// Browser-side challenge signal. Keep a typed error all the way through
/// the backend mapping rather than classifying provider-controlled text.
#[derive(Debug)]
pub struct ProviderChallengeError {
    provider: String,
}

impl ProviderChallengeError {
    pub fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
        }
    }
}

impl std::fmt::Display for ProviderChallengeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "provider '{}' disabled after challenge page detection; manual review required",
            self.provider
        )
    }
}

impl std::error::Error for ProviderChallengeError {}

fn provider_challenge_error(provider: &str) -> anyhow::Error {
    anyhow::Error::new(ProviderChallengeError::new(provider))
}

/// Typed auth failure for a redirect observed during prompt dispatch or
/// response polling. Only the structured URL classifier may make this
/// determination; query text and chat content are not authentication proof.
fn login_redirect_error(url: &str) -> Option<anyhow::Error> {
    common::is_login_url(url)
        .then(|| anyhow::Error::new(crate::browser::pool::ProviderSessionExpiredError))
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

/// The 9 supported providers (v2's enum, kept in providers:: where the
/// adapters live; browser::pool re-exports it).
#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug)]
pub enum Provider {
    Qwen,
    MetaAI,
    Claude,
    ChatGPT,
    Gemini,
    Grok,
    Manus,
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
    async fn select_model(&self, _page: &mut Page, _model: &str) -> Result<()> {
        Ok(())
    }

    /// Wait for the input, type the prompt, click send.
    async fn send_prompt(&self, page: &mut Page, prompt: &str) -> Result<()>;

    /// Current full text of the (last) response, if present.
    fn response_text(&self, page: &Page) -> Option<String>;

    /// Fetch text, node count, and streaming state in one page evaluation.
    fn response_probe(&self, page: &Page, check_body_challenge: bool) -> common::ResponseProbe {
        common::response_probe(
            page,
            &self.effective_selectors().response_refs(),
            check_body_challenge,
        )
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
        // A reused SPA can retain hidden/disabled Stop buttons, and may also
        // show unrelated controls such as "Stop sharing". Clicking the first
        // DOM match is not a reliable generation cancellation.
        let _ = page.evaluate(
            r#"(() => {
                for (const button of document.querySelectorAll('button, [role="button"]')) {
                    const label = (button.getAttribute("aria-label") || button.innerText || button.textContent || "").trim();
                    if (!/^(stop|stop generating|stop generation|stop responding|stop response|stop answering)$/i.test(label)) continue;
                    try {
                        if (!button.getClientRects().length ||
                            window.getComputedStyle(button).visibility === "hidden" ||
                            button.closest('[hidden], [inert], [aria-hidden="true"]') ||
                            button.disabled || button.matches(':disabled') ||
                            button.closest('fieldset[disabled], [aria-disabled="true"]')) continue;
                        button.click();
                        return true;
                    } catch (_) {}
                }
                return false;
            })()"#,
        );
    }

    fn selectors(&self) -> AdapterSelectors;

    /// Retained compatibility hook for adapter-specific built-in selector
    /// metadata. Runtime readiness and --test-adapter use effective_selectors
    /// so configuration overrides are never bypassed.
    #[allow(dead_code)]
    fn selector_set(&self) -> &common::SelectorSet;
    fn effective_selectors(&self) -> common::EffectiveSelectorSet;
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
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        ThreadSupport::None,
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
        ThreadSupport::None,
        |cfg| Arc::new(gemini::GeminiAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Grok,
        "grok",
        &grok::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        ThreadSupport::None,
        |cfg| Arc::new(grok::GrokAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Manus,
        "manus",
        &manus::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        // A Manus task URL is not a verified resumable chat thread.
        ThreadSupport::None,
        |cfg| Arc::new(manus::ManusAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Mistral,
        "mistral",
        &mistral::SELECTORS,
        false,
        DoneSignal::StableResponseText,
        ThreadSupport::None,
        |cfg| Arc::new(mistral::MistralAdapter { cfg }),
    ),
    adapter_spec(
        Provider::Perplexity,
        "perplexity",
        &perplexity::SELECTORS,
        true,
        DoneSignal::StableResponseText,
        ThreadSupport::None,
        |cfg| Arc::new(perplexity::PerplexityAdapter { cfg }),
    ),
];

/// Whether an adapter can safely resume a conversation by navigating to a
/// provider-owned thread URL. Sidebar-driven adapters must use replay until
/// their thread-selection behavior is implemented and fixture-tested.
pub fn supports_url_addressable_threads(provider: Provider) -> bool {
    ADAPTERS
        .iter()
        .find(|spec| spec.provider == provider)
        .is_some_and(|spec| spec.threads == ThreadSupport::UrlAddressable)
}

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
    // A byte count alone cannot detect same-length DOM rewrites. Keep the
    // last emitted snapshot to require append-only growth before completion.
    last_text: String,
}

impl ResponseDeltaTracker {
    fn new(baseline: Option<String>) -> Self {
        Self {
            baseline,
            last_text: String::new(),
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
            self.last_text = text.to_owned();
            self.baseline = None;
            return DeltaObservation::Delta(delta);
        }
        // A provider may replace or shorten partially rendered DOM text
        // without increasing its length. Never treat such rewrites as stable,
        // or append an unrelated suffix to already-streamed content. Retain
        // the last valid snapshot so a transient rerender can recover.
        if !text.starts_with(&self.last_text) {
            return DeltaObservation::InvalidBoundary;
        }
        if text.len() > self.last_text.len() {
            let delta = text[self.last_text.len()..].to_string();
            self.last_text = text.to_owned();
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

fn probe_has_challenge(probe: &common::ResponseProbe) -> bool {
    // The shared classifier only recognizes challenge hosts and path segments.
    // Never trip the sticky circuit breaker because a prompt, chat title or
    // redirect target contains the word "challenge" in URL query parameters.
    probe.challenge || crate::provider_guard::is_challenge_url(&probe.url)
}

#[cfg(test)]
mod response_poller_challenge_tests {
    use super::*;

    #[test]
    fn ignores_prompt_and_chat_title_keywords() {
        for url in [
            "https://chatgpt.com/c/challenge-essay",
            "https://gemini.google.com/app?prompt=security%20challenge",
            "https://grok.com/chat?q=captcha",
            "https://grok.com/chat?next=https%3A%2F%2Fexample.test%2Fcheckpoint",
            "https://challenges.cloudflare.com.attacker.invalid/chat",
        ] {
            assert!(
                !probe_has_challenge(&common::ResponseProbe {
                    url: url.to_owned(),
                    ..Default::default()
                }),
                "normal conversation must not trip circuit breaker: {url}"
            );
        }
    }

    #[test]
    fn recognizes_real_challenge_routes_and_dom_proof() {
        for url in [
            "https://challenges.cloudflare.com/",
            "https://example.test/cdn-cgi/challenge-platform/h/g/",
            "https://example.test/verify-human",
            "https://example.test/checkpoint?next=/chat",
        ] {
            assert!(probe_has_challenge(&common::ResponseProbe {
                url: url.to_owned(),
                ..Default::default()
            }));
        }
        assert!(probe_has_challenge(&common::ResponseProbe {
            url: "https://chatgpt.com/".to_owned(),
            challenge: true,
            ..Default::default()
        }));
    }
}

#[cfg(test)]
mod send_auth_redirect_tests {
    use super::login_redirect_error;
    use crate::browser::pool::ProviderSessionExpiredError;

    #[test]
    fn auth_routes_use_typed_session_expiry_during_submission_and_polling() {
        for url in [
            "https://chatgpt.com/auth/login?next=%2Fc",
            "https://accounts.x.com/",
            "https://login.microsoftonline.com/tenant/oauth2/authorize",
        ] {
            let error = login_redirect_error(url).expect("explicit auth redirect");
            assert!(error.downcast_ref::<ProviderSessionExpiredError>().is_some());
        }
    }

    #[test]
    fn normal_conversations_and_redirect_strings_are_not_auth_failures() {
        for url in [
            "https://chatgpt.com/c/authoring",
            "https://grok.com/chat?next=https%3A%2F%2Faccounts.x.com%2Flogin",
            "https://accounts.x.com.attacker.invalid/",
            "https://gemini.google.com/app?q=auth",
            "data:text/html,<p>Login</p>",
        ] {
            assert!(login_redirect_error(url).is_none(), "false expiry: {url}");
        }
    }
}

/// Completion is a two-snapshot decision. A new token, DOM node, or resumed
/// streaming indicator between the idle poll and the final challenge check
/// invalidates completion. Otherwise a provider can append text just after
/// the last delta and have the gateway commit a truncated turn.
fn final_probe_confirms_completion(
    initial_text: &str,
    initial_count: usize,
    final_probe: &common::ResponseProbe,
    done_signal: DoneSignal,
    saw_streaming: bool,
    confirmed_post_answer_affordance: bool,
) -> bool {
    if final_probe.streaming
        || final_probe.text.as_deref() != Some(initial_text)
        || final_probe.count != initial_count
    {
        return false;
    }
    // A fast explicit-done path must not accept a final snapshot in which the
    // completed-answer control has disappeared. Otherwise use the configured
    // conservative fallback as before.
    if done_signal == DoneSignal::StopControlDisappeared
        && saw_streaming
        && confirmed_post_answer_affordance
    {
        return final_probe.post_answer_affordance;
    }
    true
}

/// Generic send + stream-poll loop shared by all adapters. Poll response
/// text at the configured interval and finish only after a conservative
/// time-based idle grace period with no streaming indicator, or at timeout.
/// The grace period tolerates long reasoning/search pauses between deltas.
/// Runs on the browser thread; chunks leave via the channel.
#[cfg(test)]
const COMPLETION_IDLE_TICKS: u32 = 43; // 15-second grace at the default 350ms poll tick.
#[cfg(test)]
const EXPLICIT_DONE_IDLE_TICKS: u32 = 3; // One-second grace at the default poll tick.

fn confirm_consecutive_signal(previous: bool, current: bool, saw_absent_since_send: bool) -> bool {
    // A copy/feedback button left over from the previous answer must not
    // count as the terminal affordance for the new generation.
    saw_absent_since_send && previous && current
}

fn ticks_for_grace(grace: Duration, poll_tick: Duration) -> u32 {
    let tick_ms = poll_tick.as_millis().max(1);
    let grace_ms = grace.as_millis();
    grace_ms.div_ceil(tick_ms).min(u32::MAX as u128) as u32
}

fn completion_idle_ticks(
    done_signal: DoneSignal,
    saw_streaming: bool,
    post_answer_affordance: bool,
    poll_tick: Duration,
    completion_idle: Duration,
) -> u32 {
    let grace = match done_signal {
        DoneSignal::StableResponseText => completion_idle,
        DoneSignal::StopControlDisappeared if saw_streaming && post_answer_affordance => {
            Duration::from_secs(1)
        }
        // Never infer completion from disappearance alone: require a known
        // post-answer affordance, otherwise retain the configured fallback.
        DoneSignal::StopControlDisappeared => completion_idle,
    };
    ticks_for_grace(grace, poll_tick)
}

fn completion_ready(
    stable_ticks: u32,
    streaming: bool,
    done_signal: DoneSignal,
    saw_streaming: bool,
    post_answer_affordance: bool,
    poll_tick: Duration,
    completion_idle: Duration,
) -> bool {
    stable_ticks
        >= completion_idle_ticks(
            done_signal,
            saw_streaming,
            post_answer_affordance,
            poll_tick,
            completion_idle,
        )
        && !streaming
}

/// Per-request runtime policy shared with the browser send loop.
pub struct ProviderSendPolicy {
    pub completion_idle: Duration,
    pub provider_guard: crate::provider_guard::ProviderGuard,
    pub metrics: crate::metrics::MetricsRegistry,
}

pub async fn run_send(
    adapter: Arc<dyn SiteAdapter>,
    page: &mut Page,
    prompt: String,
    timeout: Duration,
    poll_tick: Duration,
    policy: ProviderSendPolicy,
    chunks: mpsc::Sender<Result<ProviderChunk>>,
) {
    let ProviderSendPolicy {
        completion_idle,
        provider_guard,
        metrics,
    } = policy;
    if crate::provider_guard::is_challenge_page(page) {
        provider_guard.disable_provider(
            adapter.name(),
            "challenge page detected; manual review required",
        );
        let _ = chunks
            .send(Err(provider_challenge_error(adapter.name())))
            .await;
        return;
    }
    if let Some(error) = login_redirect_error(&page.url()) {
        let _ = chunks.send(Err(error)).await;
        return;
    }

    // Baseline snapshot (Go concept, Rust form): capture the response
    // text BEFORE sending, so pre-existing text on a reused warm page
    // is never emitted as new. Rust v1 had no baseline (last_len
    // started at 0); this fixes that stale-text bug while keeping
    // Rust v1's 350ms / stability / indicator completion.
    let done_signal = adapter.done_signal();
    let baseline_probe = adapter.response_probe(page, true);
    let baseline_response_count = baseline_probe.count;
    let baseline = baseline_probe.text.filter(|t| !t.is_empty());
    let identical_answer_fallback = baseline.clone();
    let mut baseline = baseline;

    // Apply the same timeout budget to prompt submission and response
    // polling; otherwise a stuck selector/input action can hang indefinitely
    // before the existing polling timeout even starts.
    let start = Instant::now();
    match tokio::time::timeout(timeout, adapter.send_prompt(page, &prompt)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            // A provider can redirect to manual verification while the
            // composer is being typed or the Send control is settling.
            // Preserve the typed challenge category rather than exposing
            // a misleading missing-composer or submission error.
            if crate::provider_guard::is_challenge_page(page) {
                provider_guard.disable_provider(
                    adapter.name(),
                    "challenge page detected; manual review required",
                );
                let _ = chunks
                    .send(Err(provider_challenge_error(adapter.name())))
                    .await;
            } else if let Some(auth_error) = login_redirect_error(&page.url()) {
                let _ = chunks.send(Err(auth_error)).await;
            } else {
                let _ = chunks.send(Err(error)).await;
            }
            return;
        }
        Err(_) => {
            metrics.record_provider_timeout(adapter.name(), "submission");
            let _ = chunks
                .send(Err(anyhow::Error::new(common::ProviderTimeoutError::new(
                    adapter.name(),
                    timeout,
                    common::ProviderTimeoutPhase::Submission,
                ))))
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
    let mut previous_post_answer_affordance = false;
    let mut saw_post_answer_affordance_absent = !baseline_probe.post_answer_affordance;
    let mut response_node_detected = false;
    let mut poll_count = 0u32;

    loop {
        if chunks.is_closed() {
            adapter.cancel_generation(page);
            tracing::debug!(
                provider = adapter.name(),
                "client disconnected; requested generation stop"
            );
            return;
        }

        if start.elapsed() > timeout {
            // A timeout is not a successful completion. Propagate a typed
            // provider error so the API won't commit a partial/empty turn.
            metrics.record_provider_timeout(adapter.name(), "generation");
            // The request is terminal, but the remote composer may still be
            // generating. Stop it before another send reuses this warm page.
            // Cancellation is best effort; the timeout remains an error.
            adapter.cancel_generation(page);
            let _ = chunks
                .send(Err(anyhow::Error::new(common::ProviderTimeoutError::new(
                    adapter.name(),
                    timeout,
                    common::ProviderTimeoutPhase::Generation,
                ))))
                .await;
            // Do not fall through to the unconditional "generation complete"
            // log after a terminal error. An expired response is never done.
            return;
        }

        common::drive_page(page, poll_tick).await;
        let poll_started = Instant::now();
        poll_count = poll_count.saturating_add(1);
        // Body text extraction is expensive on large pages; keep URL checks on every
        // tick, but inspect challenge text every five polls (~1.75 seconds).
        let check_body_challenge = should_check_body_challenge(poll_count);
        let probe = adapter.response_probe(page, check_body_challenge);
        metrics.record_browser_poll(
            adapter.name(),
            1,
            poll_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        );
        if probe_has_challenge(&probe) {
            provider_guard.disable_provider(
                adapter.name(),
                "challenge page detected; manual review required",
            );
            let _ = chunks
                .send(Err(provider_challenge_error(adapter.name())))
                .await;
            return;
        }
        // A login redirect after Send must not look like a slow model or
        // allow the previous warm-page response to complete successfully.
        if let Some(auth_error) = login_redirect_error(&probe.url) {
            let _ = chunks.send(Err(auth_error)).await;
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
        if !probe.post_answer_affordance {
            saw_post_answer_affordance_absent = true;
        }
        // Only observations after the stop control disappears count toward
        // the two-tick terminal signal. Earlier buttons may belong to a
        // previous response, or coexist with an ongoing generation.
        let current_completed_affordance = !streaming && probe.post_answer_affordance;
        let confirmed_post_answer_affordance = confirm_consecutive_signal(
            previous_post_answer_affordance,
            current_completed_affordance,
            saw_post_answer_affordance_absent,
        );
        previous_post_answer_affordance = current_completed_affordance;

        if let Some(txt) = probe.text {
            match tracker.observe(&txt) {
                DeltaObservation::NoText | DeltaObservation::InvalidBoundary => {
                    // Only consecutive, valid response snapshots can accrue
                    // completion confidence. A transient empty/shrunk view
                    // invalidates both the idle grace and fast-done signal.
                    stable_ticks = 0;
                    previous_post_answer_affordance = false;
                    continue;
                }
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
                    if stable_ticks
                        >= completion_idle_ticks(
                            done_signal,
                            saw_streaming,
                            confirmed_post_answer_affordance,
                            poll_tick,
                            completion_idle,
                        )
                    {
                        // Sample the full body immediately before completion, even
                        // when this poll is not one of the periodic body checks.
                        let final_probe_started = Instant::now();
                        let final_probe = adapter.response_probe(page, true);
                        metrics.record_browser_poll(
                            adapter.name(),
                            1,
                            final_probe_started
                                .elapsed()
                                .as_millis()
                                .min(u64::MAX as u128) as u64,
                        );
                        if probe_has_challenge(&final_probe) {
                            provider_guard.disable_provider(
                                adapter.name(),
                                "challenge page detected; manual review required",
                            );
                            let _ = chunks
                                .send(Err(provider_challenge_error(adapter.name())))
                                .await;
                            return;
                        }
                        if !final_probe_confirms_completion(
                            &txt,
                            probe.count,
                            &final_probe,
                            done_signal,
                            saw_streaming,
                            confirmed_post_answer_affordance,
                        ) {
                            // Continue polling rather than emitting the old
                            // answer or an incomplete terminal event.
                            stable_ticks = 0;
                            continue;
                        }
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
                                adapter.cancel_generation(page);
                                return;
                            }
                        }
                        if let Some(last_delta_at) = last_delta_at {
                            metrics.record_provider_tail(
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
                        adapter.cancel_generation(page);
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

            // Require a stopped indicator and stable text. Providers with a
            // verified post-answer affordance can finish after a short grace;
            // all other cases retain the conservative ~15s fallback.
            if completion_ready(
                stable_ticks,
                streaming,
                done_signal,
                saw_streaming,
                confirmed_post_answer_affordance,
                poll_tick,
                completion_idle,
            ) {
                // A final full-body check closes the gap between periodic
                // sampling and declaring the provider turn complete.
                let final_probe_started = Instant::now();
                let final_probe = adapter.response_probe(page, true);
                metrics.record_browser_poll(
                    adapter.name(),
                    1,
                    final_probe_started
                        .elapsed()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                );
                if probe_has_challenge(&final_probe) {
                    provider_guard.disable_provider(
                        adapter.name(),
                        "challenge page detected; manual review required",
                    );
                    let _ = chunks
                        .send(Err(provider_challenge_error(adapter.name())))
                        .await;
                    return;
                }
                if !final_probe_confirms_completion(
                    &txt,
                    probe.count,
                    &final_probe,
                    done_signal,
                    saw_streaming,
                    confirmed_post_answer_affordance,
                ) {
                    stable_ticks = 0;
                    continue;
                }
                if let Some(last_delta_at) = last_delta_at {
                    metrics.record_provider_tail(
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
        } else {
            // DOM rerendering can temporarily hide the response altogether.
            // Do not preserve idle ticks or a fast-done signal across this gap.
            stable_ticks = 0;
            previous_post_answer_affordance = false;
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
    fn same_length_dom_rewrite_is_not_a_stable_response() {
        let mut tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("alpha"),
            DeltaObservation::Delta("alpha".to_string())
        );
        assert_eq!(tracker.observe("bravo"), DeltaObservation::InvalidBoundary);
        assert_eq!(tracker.observe("bravo"), DeltaObservation::InvalidBoundary);
        // Only the original append-only text can resume a valid stream.
        assert_eq!(
            tracker.observe("alpha!"),
            DeltaObservation::Delta("!".to_string())
        );
        assert_eq!(tracker.observe("alpha!"), DeltaObservation::Stable);
    }

    #[test]
    fn longer_dom_rewrite_must_not_append_suffix_to_prior_output() {
        let mut tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("short"),
            DeltaObservation::Delta("short".to_string())
        );
        assert_eq!(
            tracker.observe("rewritten answer"),
            DeltaObservation::InvalidBoundary
        );
        assert_eq!(
            tracker.observe("short answer"),
            DeltaObservation::Delta(" answer".to_string())
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
        completion_idle_ticks, completion_ready, ticks_for_grace, DoneSignal,
        COMPLETION_IDLE_TICKS, EXPLICIT_DONE_IDLE_TICKS,
    };
    use std::time::Duration;

    const DEFAULT_POLL_TICK: Duration = Duration::from_millis(350);

    #[test]
    fn shortened_or_missing_response_cannot_be_mistaken_for_stable_completion() {
        use super::{DeltaObservation, ResponseDeltaTracker};
        let mut tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("long partial response"),
            DeltaObservation::Delta("long partial response".to_owned())
        );
        assert_eq!(tracker.observe("short"), DeltaObservation::InvalidBoundary);
        assert_eq!(tracker.observe(""), DeltaObservation::NoText);
        assert_eq!(
            tracker.observe("long partial response"),
            DeltaObservation::Stable,
            "only the original full snapshot may be considered stable"
        );
    }

    #[test]
    fn stable_text_requires_conservative_idle_grace() {
        assert!(!completion_ready(
            28,
            false,
            DoneSignal::StableResponseText,
            true,
            false,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
        assert!(!completion_ready(
            COMPLETION_IDLE_TICKS - 1,
            false,
            DoneSignal::StableResponseText,
            true,
            false,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
        assert!(completion_ready(
            COMPLETION_IDLE_TICKS,
            false,
            DoneSignal::StableResponseText,
            true,
            false,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
    }

    #[test]
    fn active_streaming_prevents_completion_even_after_idle_ticks() {
        assert!(!completion_ready(
            COMPLETION_IDLE_TICKS + 10,
            true,
            DoneSignal::StableResponseText,
            true,
            false,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
    }

    #[test]
    fn post_answer_affordance_requires_two_consecutive_observations() {
        use super::confirm_consecutive_signal;

        assert!(!confirm_consecutive_signal(false, true, true));
        assert!(!confirm_consecutive_signal(true, false, true));
        assert!(!confirm_consecutive_signal(false, false, true));
        assert!(confirm_consecutive_signal(true, true, true));
        assert!(
            !confirm_consecutive_signal(true, true, false),
            "stale buttons already visible before a new send are not a fresh done signal"
        );
    }

    #[test]
    fn ten_second_pause_without_fresh_done_signal_does_not_complete_early() {
        // A long reasoning/tool pause may remove the streaming indicator.
        // Without a fresh two-tick post-answer signal, the full fallback
        // grace must still apply even after the stop button disappears.
        for tick in 0..=29 {
            assert!(!completion_ready(
                tick,
                false,
                DoneSignal::StopControlDisappeared,
                true,
                false,
                DEFAULT_POLL_TICK,
                Duration::from_millis(15_000),
            ));
        }
        assert!(completion_ready(
            COMPLETION_IDLE_TICKS,
            false,
            DoneSignal::StopControlDisappeared,
            true,
            false,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
    }

    #[test]
    fn explicit_stop_disappearance_uses_short_grace_only_after_streaming_was_seen() {
        assert_eq!(
            completion_idle_ticks(
                DoneSignal::StopControlDisappeared,
                true,
                true,
                DEFAULT_POLL_TICK,
                Duration::from_millis(15_000),
            ),
            EXPLICIT_DONE_IDLE_TICKS,
        );
        assert_eq!(
            completion_idle_ticks(
                DoneSignal::StopControlDisappeared,
                false,
                true,
                DEFAULT_POLL_TICK,
                Duration::from_millis(15_000),
            ),
            COMPLETION_IDLE_TICKS,
        );
        assert_eq!(
            completion_idle_ticks(
                DoneSignal::StopControlDisappeared,
                true,
                false,
                DEFAULT_POLL_TICK,
                Duration::from_millis(15_000),
            ),
            COMPLETION_IDLE_TICKS,
            "missing post-answer affordance must use the conservative fallback",
        );
        assert!(!completion_ready(
            EXPLICIT_DONE_IDLE_TICKS - 1,
            false,
            DoneSignal::StopControlDisappeared,
            true,
            true,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
        assert!(completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            false,
            DoneSignal::StopControlDisappeared,
            true,
            true,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
        assert!(!completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            true,
            DoneSignal::StopControlDisappeared,
            true,
            true,
            DEFAULT_POLL_TICK,
            Duration::from_millis(15_000),
        ));
    }

    #[test]
    fn final_probe_rejects_late_tokens_new_nodes_and_resumed_streaming() {
        use super::final_probe_confirms_completion;
        use crate::providers::common::ResponseProbe;

        let initial = ResponseProbe {
            text: Some("first part".to_owned()),
            count: 3,
            streaming: false,
            post_answer_affordance: true,
            ..ResponseProbe::default()
        };
        let stable = initial.clone();
        assert!(final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            &stable,
            DoneSignal::StopControlDisappeared,
            true,
            true,
        ));

        let mut late_token = stable.clone();
        late_token.text = Some("first part and more".to_owned());
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            &late_token,
            DoneSignal::StopControlDisappeared,
            true,
            true,
        ));

        let mut new_node = stable.clone();
        new_node.count += 1;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            &new_node,
            DoneSignal::StableResponseText,
            false,
            false,
        ));

        let mut resumed = stable.clone();
        resumed.streaming = true;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            &resumed,
            DoneSignal::StableResponseText,
            true,
            false,
        ));

        let mut gone = stable.clone();
        gone.text = None;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            &gone,
            DoneSignal::StableResponseText,
            false,
            false,
        ));
    }

    #[test]
    fn explicit_done_final_probe_requires_persistent_post_answer_affordance() {
        use super::final_probe_confirms_completion;
        use crate::providers::common::ResponseProbe;

        let initial = ResponseProbe {
            text: Some("complete response".into()),
            count: 1,
            post_answer_affordance: true,
            ..ResponseProbe::default()
        };
        let mut final_probe = initial.clone();
        final_probe.post_answer_affordance = false;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            &final_probe,
            DoneSignal::StopControlDisappeared,
            true,
            true,
        ));
        // Providers without a validated fast-done signal may still finish
        // after the full configured fallback grace.
        assert!(final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            &final_probe,
            DoneSignal::StableResponseText,
            true,
            false,
        ));
    }

    #[test]
    fn completion_grace_duration_is_independent_of_poll_interval() {
        assert_eq!(
            ticks_for_grace(Duration::from_secs(15), DEFAULT_POLL_TICK),
            43
        );
        assert_eq!(
            ticks_for_grace(Duration::from_secs(1), DEFAULT_POLL_TICK),
            3
        );
        assert_eq!(
            ticks_for_grace(Duration::from_secs(15), Duration::from_millis(200)),
            75
        );
        assert_eq!(
            ticks_for_grace(Duration::from_secs(1), Duration::from_millis(200)),
            5
        );
    }
}

#[cfg(test)]
mod thread_support_tests {
    use super::{supports_url_addressable_threads, Provider};

    #[test]
    fn url_reopen_capability_comes_from_adapter_catalog() {
        for provider in [Provider::ChatGPT, Provider::Claude] {
            assert!(supports_url_addressable_threads(provider), "{provider:?}");
        }
        for provider in [
            Provider::Gemini,
            Provider::Grok,
            Provider::Manus,
            Provider::Qwen,
            Provider::Mistral,
            Provider::Perplexity,
            Provider::MetaAI,
        ] {
            assert!(
                !supports_url_addressable_threads(provider),
                "unverified thread navigation must use replay: {provider:?}"
            );
        }
    }
}

#[cfg(test)]
mod provider_catalog_contract_tests {
    use super::{DoneSignal, Provider, ADAPTERS};
    use std::collections::HashSet;

    #[test]
    fn provider_catalog_names_are_unique_and_round_trip_exactly() {
        let providers = Provider::all();
        assert_eq!(providers.len(), 9, "the public provider catalog changed");

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
        assert_eq!(ADAPTERS.len(), 9);
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
    fn only_validated_catalog_entries_use_explicit_done_signals() {
        for spec in ADAPTERS {
            let expected = match spec.id {
                // These adapters have the explicit stop-control disappearance
                // plus post-answer-affordance completion contract.
                "claude" | "chatgpt" => DoneSignal::StopControlDisappeared,
                // Keep unvalidated providers on the conservative idle fallback.
                _ => DoneSignal::StableResponseText,
            };
            assert_eq!(
                spec.done, expected,
                "unexpected completion strategy for provider {}",
                spec.id
            );
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

#[cfg(test)]
mod completion_pause_regression_tests {
    use super::{
        completion_ready, DeltaObservation, DoneSignal, ResponseDeltaTracker, COMPLETION_IDLE_TICKS,
    };
    use std::time::Duration;

    const POLL: Duration = Duration::from_millis(350);

    #[test]
    fn ten_second_mid_stream_pause_does_not_complete_a_response() {
        // 10 seconds is 29 polling intervals at the production cadence;
        // the conservative fallback must remain incomplete until ~15 seconds.
        let mut tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("partial answer"),
            DeltaObservation::Delta("partial answer".into())
        );
        let stable_ticks_during_pause = 29;
        assert_eq!(tracker.observe("partial answer"), DeltaObservation::Stable);
        assert!(!completion_ready(
            stable_ticks_during_pause,
            false,
            DoneSignal::StableResponseText,
            true,
            false,
            POLL,
            Duration::from_millis(15_000),
        ));
        assert!(completion_ready(
            COMPLETION_IDLE_TICKS,
            false,
            DoneSignal::StableResponseText,
            true,
            false,
            POLL,
            Duration::from_millis(15_000),
        ));
    }

    #[test]
    fn identical_consecutive_answers_wait_for_a_new_turn_idle_grace() {
        let mut tracker = ResponseDeltaTracker::new(Some("same answer".into()));
        assert_eq!(
            tracker.observe("same answer"),
            DeltaObservation::WaitingForNewResponse
        );
        // The reused DOM node may retain identical text; only the full
        // conservative idle window may release the previous answer as fallback.
        assert!(!completion_ready(
            COMPLETION_IDLE_TICKS - 1,
            false,
            DoneSignal::StableResponseText,
            true,
            false,
            POLL,
            Duration::from_millis(15_000),
        ));
        assert!(completion_ready(
            COMPLETION_IDLE_TICKS,
            false,
            DoneSignal::StableResponseText,
            true,
            false,
            POLL,
            Duration::from_millis(15_000),
        ));
    }
}

/// Synthetic, credential-free DOM contracts. These encode the selectors
/// documented in our adapter catalog; they are not live provider recordings.
/// Keep #814 open until delegated merged-main provider acceptance is recorded.
#[cfg(test)]
mod nine_provider_selector_fixture_tests {
    use super::{common, ADAPTERS};

    #[tokio::test]
    async fn every_catalog_provider_discovers_input_send_and_response_in_its_fixture() {
        let fixtures: &[(&str, &str, &str, &str)] = &[
            ("qwen", "<textarea placeholder='Ask Qwen'></textarea>", "<button aria-label='Send message'>Send</button>", "<div class='markdown-body'>SYNTHETIC_RESPONSE</div>"),
            ("metaai", "<div contenteditable='true' role='textbox'></div>", "<button aria-label='Send'>Send</button>", "<div data-testid='bot-message'>SYNTHETIC_RESPONSE</div>"),
            ("claude", "<div contenteditable='true' data-placeholder='Ask Claude'></div>", "<button aria-label='Send Message'>Send</button>", "<div data-testid='assistant-message'>SYNTHETIC_RESPONSE</div>"),
            ("chatgpt", "<textarea id='prompt-textarea'></textarea>", "<button data-testid='send-button'>Send</button>", "<div data-message-author-role='assistant'><div class='markdown'>SYNTHETIC_RESPONSE</div></div>"),
            ("gemini", "<rich-textarea aria-label='Prompt'></rich-textarea>", "<button aria-label='Send message'>Send</button>", "<model-response>SYNTHETIC_RESPONSE</model-response>"),
            ("grok", "<textarea placeholder='Ask Grok'></textarea>", "<button aria-label='Send'>Send</button>", "<div data-testid='grok-response'>SYNTHETIC_RESPONSE</div>"),
            ("manus", "<textarea placeholder='Ask Manus'></textarea>", "<button aria-label='Send'>Send</button>", "<div data-message-role='assistant'>SYNTHETIC_RESPONSE</div>"),
            ("mistral", "<textarea placeholder='Ask Le Chat'></textarea>", "<button aria-label='Send'>Send</button>", "<div class='markdown'>SYNTHETIC_RESPONSE</div>"),
            ("perplexity", "<textarea placeholder='Ask question'></textarea>", "<button aria-label='Submit'>Submit</button>", "<div data-testid='answer'>SYNTHETIC_RESPONSE</div>"),
        ];
        assert_eq!(fixtures.len(), ADAPTERS.len());
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        for (name, input, send, response) in fixtures {
            let spec = ADAPTERS
                .iter()
                .find(|spec| spec.id == *name)
                .expect("fixture provider must exist in catalog");
            let html = format!("<html><body>{input}{send}{response}</body></html>");
            let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
            let mut page = browser.new_page().await.unwrap();
            page.goto(&format!("data:text/html,{encoded}"))
                .await
                .unwrap();

            let groups = [
                ("input", spec.selectors.input),
                ("send", spec.selectors.send),
                ("response", spec.selectors.response),
            ];
            let observed = common::probe_selector_groups(&page, &groups);
            for (role, matched) in observed {
                assert!(
                    matched.is_some(),
                    "{name}: {role} selectors did not match the fixture"
                );
            }
            assert_eq!(
                common::response_text(&page, spec.selectors.response).as_deref(),
                Some("SYNTHETIC_RESPONSE"),
                "{name}: response extraction did not match the fixture"
            );
        }
    }

    #[test]
    fn all_catalog_providers_have_nonempty_distinct_selector_roles() {
        for spec in ADAPTERS {
            for (role, selectors) in [
                ("input", spec.selectors.input),
                ("send", spec.selectors.send),
                ("response", spec.selectors.response),
            ] {
                assert!(
                    !selectors.is_empty(),
                    "{} missing {} selectors",
                    spec.id,
                    role
                );
                let unique: std::collections::HashSet<_> = selectors.iter().collect();
                assert_eq!(
                    unique.len(),
                    selectors.len(),
                    "{} has repeated {} selectors",
                    spec.id,
                    role
                );
                assert!(
                    selectors.iter().all(|selector| !selector.trim().is_empty()),
                    "{} has empty {} selector",
                    spec.id,
                    role
                );
            }
        }
    }
}

#[cfg(test)]
mod generation_cancel_visibility_tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_skips_hidden_disabled_and_unrelated_stop_buttons() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><button aria-label="Stop generating" style="display:none" onclick="window.hiddenClicks=(window.hiddenClicks||0)+1">Hidden</button><button aria-label="Stop sharing" onclick="window.unrelatedClicks=(window.unrelatedClicks||0)+1">Stop sharing</button><button aria-label="Stop generating" disabled onclick="window.disabledClicks=(window.disabledClicks||0)+1">Disabled</button><fieldset disabled><button aria-label="Stop generating" onclick="window.fieldsetClicks=(window.fieldsetClicks||0)+1">Disabled fieldset</button></fieldset><button aria-label="Stop generating" onclick="window.activeClicks=(window.activeClicks||0)+1">Stop</button></body></html>"#;
        let encoded: String = html.bytes().map(|b| format!("%{b:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let adapter = qwen::QwenAdapter {
            cfg: serde_yaml::from_str("name: qwen").unwrap(),
        };
        adapter.cancel_generation(&page);
        assert_eq!(
            page.evaluate("window.activeClicks || 0").as_f64(),
            Some(1.0)
        );
        assert_eq!(
            page.evaluate("window.hiddenClicks || 0").as_f64(),
            Some(0.0)
        );
        assert_eq!(
            page.evaluate("window.disabledClicks || 0").as_f64(),
            Some(0.0)
        );
        assert_eq!(
            page.evaluate("window.unrelatedClicks || 0").as_f64(),
            Some(0.0)
        );
        assert_eq!(
            page.evaluate("window.fieldsetClicks || 0").as_f64(),
            Some(0.0)
        );
    }
}

#[cfg(test)]
mod timeout_terminal_regression_tests {
    use super::*;

    #[tokio::test]
    async fn generation_timeout_emits_only_a_typed_error_and_phase_metric() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form><textarea placeholder="Ask anything"></textarea><button type="button" aria-label="Send" onclick="document.querySelector('#stop').hidden=false">Send</button><button type="button" id="stop" aria-label="Stop generation" hidden onclick="window.cancelCount=(window.cancelCount||0)+1">Stop</button></form></body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let adapter = Arc::new(qwen::QwenAdapter {
            cfg: serde_yaml::from_str("name: qwen\nmodels: [qwen]").unwrap(),
        });
        let metrics = crate::metrics::MetricsRegistry::default();
        let (tx, mut rx) = mpsc::channel(8);
        tokio::time::timeout(
            Duration::from_secs(8),
            run_send(
                adapter,
                &mut page,
                "SYNTHETIC_TIMEOUT".into(),
                Duration::from_secs(3),
                Duration::from_millis(50),
                ProviderSendPolicy {
                    completion_idle: Duration::from_millis(100),
                    provider_guard: crate::provider_guard::ProviderGuard::default(),
                    metrics: metrics.clone(),
                },
                tx,
            ),
        )
        .await
        .unwrap();

        let failure = rx
            .recv()
            .await
            .expect("timeout error was not sent")
            .unwrap_err();
        assert!(failure
            .downcast_ref::<common::ProviderTimeoutError>()
            .is_some());
        assert!(failure.to_string().contains("did not complete"));
        assert!(rx.recv().await.is_none(), "timeout must not emit done");
        assert_eq!(
            page.evaluate("window.cancelCount || 0").as_f64(),
            Some(1.0),
            "generation timeout should attempt to stop the in-flight provider response"
        );
        let output = metrics.render_with_provider_states(&[]);
        assert!(output
            .contains("gateway_provider_timeout_total{provider=\"qwen\",phase=\"generation\"} 1"));
        assert!(!output
            .contains("gateway_provider_timeout_total{provider=\"qwen\",phase=\"submission\"} 1"));
    }
    #[tokio::test]
    async fn challenge_during_typing_returns_typed_manual_blocker() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><textarea placeholder="Ask anything" oninput="document.body.insertAdjacentHTML('beforeend','<p>Verify you are human</p>'); this.remove()"></textarea></body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();

        let adapter = Arc::new(qwen::QwenAdapter {
            cfg: serde_yaml::from_str("name: qwen\nmodels: [qwen]").unwrap(),
        });
        let guard = crate::provider_guard::ProviderGuard::default();
        let (tx, mut rx) = mpsc::channel(8);
        tokio::time::timeout(
            Duration::from_secs(5),
            run_send(
                adapter,
                &mut page,
                "synthetic prompt".into(),
                Duration::from_secs(3),
                Duration::from_millis(50),
                ProviderSendPolicy {
                    completion_idle: Duration::from_millis(100),
                    provider_guard: guard.clone(),
                    metrics: crate::metrics::MetricsRegistry::default(),
                },
                tx,
            ),
        )
        .await
        .unwrap();

        let error = rx
            .recv()
            .await
            .expect("expected a challenge error")
            .unwrap_err();
        assert!(
            error.downcast_ref::<ProviderChallengeError>().is_some(),
            "the challenge during typing must not look like a generic composer failure"
        );
        assert_eq!(
            guard.provider_disabled_reason("qwen"),
            Some("challenge page detected; manual review required")
        );
        assert!(rx.recv().await.is_none(), "challenge must not emit done");
    }
}

#[cfg(test)]
mod live_dom_send_tests {
    use super::*;
    async fn exercise_composer(adapter: Arc<dyn SiteAdapter>, markup: &str, input: &str) {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let button = if input == "#mobile-composer-prompt" {
            r#"<button id="fixture-send" type="button" aria-label="Send message">Send</button>"#
        } else {
            r#"<button id="fixture-send" type="button" data-testid="send-button" aria-label="Send">Send</button>"#
        };
        let html = format!(
            r#"<html><body><script>globalThis.fixtureLoaded=true</script><form><button id="unrelated-send" type="button" aria-label="Send message">Other</button></form><form>{markup}{button}</form></body></html>"#
        );
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let js = r#"(() => {
          globalThis.unrelatedClicked = false;
          document.getElementById('unrelated-send').addEventListener('click', () => {globalThis.unrelatedClicked=true;});
          document.getElementById('fixture-send').addEventListener('click', () => {
            globalThis.fixtureClicked = true;
            setTimeout(() => {
              globalThis.fixtureTimerFired = true;
              const outer = document.createElement('div');
              outer.setAttribute('data-message-author-role', 'assistant');
              const text = document.createElement('div'); text.setAttribute('class', 'markdown');
              text.textContent = document.querySelector(__INPUT__).value;
              outer.appendChild(text); document.body.appendChild(outer);
            }, 30);
          });
          return true;
        })()"#
            .replace("__INPUT__", &serde_json::to_string(input).unwrap());
        assert_eq!(page.evaluate(&js), serde_json::json!(true));
        let (tx, mut rx) = mpsc::channel(32);
        tokio::time::timeout(
            Duration::from_secs(5),
            run_send(
                adapter,
                &mut page,
                "SYNTHETIC_ASYNC_OK".into(),
                Duration::from_secs(4),
                Duration::from_millis(50),
                ProviderSendPolicy {
                    completion_idle: Duration::from_millis(100),
                    provider_guard: crate::provider_guard::ProviderGuard::default(),
                    metrics: crate::metrics::MetricsRegistry::default(),
                },
                tx,
            ),
        )
        .await
        .unwrap();
        let mut text = String::new();
        let mut done = false;
        while let Some(chunk) = rx.recv().await {
            let chunk = chunk.unwrap();
            text.push_str(&chunk.content);
            done |= chunk.done;
        }
        assert_eq!(text, "SYNTHETIC_ASYNC_OK");
        assert!(done);
        assert_eq!(
            page.evaluate("globalThis.unrelatedClicked"),
            serde_json::json!(false)
        );
        assert_eq!(page.evaluate("document.getElementById('autosizer') ? document.getElementById('autosizer').value : 'DO_NOT_TOUCH'"),serde_json::json!("DO_NOT_TOUCH"));
    }
    #[tokio::test]
    async fn real_adapter_observes_asynchronous_response_in_the_pinned_browser() {
        let adapter = Arc::new(chatgpt::ChatGPTAdapter {
            cfg: serde_yaml::from_str("name: chatgpt\nmodels: [gpt]").unwrap(),
        });
        exercise_composer(
            adapter,
            r#"<textarea id="prompt-textarea"></textarea>"#,
            "#prompt-textarea",
        )
        .await;
    }
    #[tokio::test]
    async fn current_chatgpt_mobile_composer_produces_a_real_adapter_response() {
        let adapter = Arc::new(chatgpt::ChatGPTAdapter {
            cfg: serde_yaml::from_str("name: chatgpt\nmodels: [gpt]").unwrap(),
        });
        exercise_composer(adapter,r#"<textarea id="mobile-composer-prompt" name="prompt" placeholder="Ask ChatGPT" aria-label="Chat with ChatGPT"></textarea>"#,"#mobile-composer-prompt").await;
    }
    #[tokio::test]
    async fn current_grok_labelled_composer_avoids_the_hidden_autosizer() {
        let adapter = Arc::new(grok::GrokAdapter {
            cfg: serde_yaml::from_str("name: grok\nmodels: [grok]").unwrap(),
        });
        exercise_composer(adapter,r#"<textarea id="autosizer" aria-hidden="true" style="visibility:hidden">DO_NOT_TOUCH</textarea><textarea aria-label="Ask Grok anything"></textarea>"#,r#"textarea[aria-label="Ask Grok anything"]"#).await;
    }
}
