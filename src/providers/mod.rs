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
fn login_redirect_error(provider: &str, url: &str) -> Option<anyhow::Error> {
    common::login_url_matches_provider(provider, url)
        .then(|| anyhow::Error::new(crate::browser::pool::ProviderSessionExpiredError))
}

/// Both the periodic and final completion samples must classify a new
/// first-party authentication redirect. A stable old SPA reply may otherwise
/// survive in the same origin while the route changes to /auth/login.
fn probe_login_redirect_error(
    provider: &str,
    probe: &common::ResponseProbe,
) -> Option<anyhow::Error> {
    login_redirect_error(provider, &probe.url)
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
        // The provider-origin check and the Stop click must run within
        // one DOM evaluation, not a Rust URL read followed by a later click.
        let route_guard = match common::provider_origin_guard_js(Some(self.name())) {
            Ok(guard) => guard,
            Err(_) => return,
        };
        // Test-only escape hatch for explicitly tagged offline data fixtures;
        // the production build always requires the strict HTTPS origin.
        #[cfg(test)]
        let route_guard = format!(
            "if (!(location.protocol === 'data:' && document.documentElement.hasAttribute('data-chat-gateway-test-fixture'))) {{ {route_guard} }}"
        );
        let js = r#"(() => {
                __CG_EXPECTED_PROVIDER_ORIGIN_GUARD__
                for (const button of document.querySelectorAll('button, [role="button"]')) {
                    const label = (button.getAttribute("aria-label") || button.innerText || button.textContent || "").trim();
                    if (!/^(stop|stop generating|stop generation|stop responding|stop response|stop answering)$/i.test(label)) continue;
                    try {
                        if (!button.getClientRects().length ||
                            window.getComputedStyle(button).visibility === "hidden" ||
                            button.closest('[hidden], [inert], [aria-hidden="true"]') ||
                            button.disabled || button.matches(':disabled') ||
                            button.closest('fieldset[disabled], [aria-disabled="true"]')) continue;
                        // A hidden CSS or pointer-inert previous-turn Stop
                        // clone may still receive programmatic click(), then
                        // shadow the real action later in document order.
                        let actionable = true;
                        for (let ancestor = button; ancestor; ancestor = ancestor.parentElement) {
                            const style = window.getComputedStyle(ancestor);
                            if (style.pointerEvents === "none" || Number(style.opacity) === 0) {
                                actionable = false;
                                break;
                            }
                        }
                        if (!actionable) continue;
                        button.click();
                        return true;
                    } catch (_) {}
                }
                return false;
            })()"#
            .replace("__CG_EXPECTED_PROVIDER_ORIGIN_GUARD__", &route_guard);
        let _ = page.evaluate(&js);
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

/// Incrementally remove provider-supplied numeric citations without making
/// output depend on DOM polling boundaries. Retain only an incomplete trailing
/// bracket-and-digits candidate, never the entire provider response.
struct CitationDeltaFilter {
    pending: String,
    suffix: regex::Regex,
}

impl CitationDeltaFilter {
    // Large malformed/unfinished bracket sequences must not buffer arbitrary
    // response text while waiting for a closing bracket.
    const MAX_PENDING_BYTES: usize = 128;

    fn new() -> Self {
        Self {
            pending: String::new(),
            suffix: regex::Regex::new(r"\[\p{Nd}*$").expect("static citation suffix regex"),
        }
    }

    fn push(&mut self, delta: &str, complete: &regex::Regex) -> String {
        let mut combined = std::mem::take(&mut self.pending);
        combined.push_str(delta);
        if let Some(candidate) = self.suffix.find(&combined) {
            if combined.len() - candidate.start() <= Self::MAX_PENDING_BYTES {
                self.pending = combined.split_off(candidate.start());
            }
        }
        complete.replace_all(&combined, "").into_owned()
    }

    /// An unfinished citation marker is literal content, not a matched
    /// citation. Flush it before sending the successful terminal chunk.
    fn finish(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod citation_delta_filter_tests {
    use super::CitationDeltaFilter;

    fn collect(parts: &[&str]) -> String {
        let complete = regex::Regex::new(r"\[\d+\]").unwrap();
        let mut filter = CitationDeltaFilter::new();
        let mut output = String::new();
        for part in parts {
            output.push_str(&filter.push(part, &complete));
        }
        output.push_str(&filter.finish());
        output
    }

    #[test]
    fn numeric_citations_are_removed_across_chunk_boundaries() {
        for chunks in [
            vec!["See [", "12]", " text [", "3]", " done"],
            vec!["See [12] text [3] done"],
            vec!["See ", "[1", "2] text ", "[3", "] done"],
        ] {
            assert_eq!(collect(&chunks), "See  text  done");
        }
    }

    #[test]
    fn non_citation_text_and_incomplete_brackets_remain_exact() {
        assert_eq!(collect(&["arr[", "abc] and [", "42"]), "arr[abc] and [42");
        assert_eq!(collect(&["plain ", "[", "x]"]), "plain [x]");
        assert_eq!(collect(&["text ", "["]), "text [");
    }

    #[test]
    fn unicode_decimal_citations_use_the_same_regex_semantics() {
        assert_eq!(collect(&["before [", "١", "٢] after"]), "before  after");
        assert_eq!(collect(&["A[1] B[", "٣]"]), "A B");
    }

    #[test]
    fn lookbehind_is_bounded_and_overlong_unclosed_candidate_is_literal() {
        let digits = "7".repeat(CitationDeltaFilter::MAX_PENDING_BYTES + 12);
        let original = format!("hello [{digits}");
        let parts = ["hello [", digits.as_str()];
        assert_eq!(collect(&parts), original);
        let complete = regex::Regex::new(r"\[\d+\]").unwrap();
        let mut filter = CitationDeltaFilter::new();
        let output = filter.push(&original, &complete);
        assert_eq!(output, original);
        assert!(filter.pending.is_empty());
    }
}

fn is_new_response_node(baseline_count: usize, current_count: usize) -> bool {
    current_count > baseline_count
}

/// A blank newly rendered assistant placeholder raises the total count but
/// leaves the last populated reply pointing at the PREVIOUS turn.
fn has_unpopulated_new_response_placeholder(
    baseline_count: usize,
    current_count: usize,
    baseline_populated_count: usize,
    current_populated_count: usize,
) -> bool {
    is_new_response_node(baseline_count, current_count)
        && current_populated_count <= baseline_populated_count
}

/// Reset only after a genuinely populated new response root arrives.
/// Reused-node responses still use the existing baseline / streaming contract.
fn should_reset_response_tracker(
    already_detected: bool,
    baseline_count: usize,
    current_count: usize,
    baseline_populated_count: usize,
    current_populated_count: usize,
) -> bool {
    !already_detected
        && is_new_response_node(baseline_count, current_count)
        && current_populated_count > baseline_populated_count
}

/// If an empty placeholder was observed, its disappearance must not revive
/// the earlier answer as a completion after a transient Stop indicator.
fn stale_baseline_after_disappearing_placeholder(
    saw_placeholder: bool,
    populated_new_root_detected: bool,
    old_text: Option<&str>,
    current_text: Option<&str>,
) -> bool {
    saw_placeholder && !populated_new_root_detected && current_text == old_text
}

/// Avoid evaluating the DOM for response-node counts after the new response
/// node has already been detected; the count remains elevated on every poll.
fn should_probe_response_count(response_node_detected: bool) -> bool {
    !response_node_detected
}

/// An inherited Stop control at the baseline cannot prove the next request
/// generated the same text again. Require an observable inactive->active
/// transition before allowing the identical-answer fallback. This is a
/// bounded safety signal, not full accepted-turn or DOM root identity.
#[derive(Debug, Clone, Copy)]
struct FreshStopSignal {
    saw_inactive: bool,
    saw_fresh_active: bool,
}

impl FreshStopSignal {
    fn new(baseline_active: bool) -> Self {
        Self {
            saw_inactive: !baseline_active,
            saw_fresh_active: false,
        }
    }

    fn observe(&mut self, active: bool) {
        if !active {
            self.saw_inactive = true;
        } else if self.saw_inactive {
            self.saw_fresh_active = true;
        }
    }

    fn saw_fresh_generation(&self) -> bool {
        self.saw_fresh_active
    }
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
            let error = login_redirect_error("chatgpt", url).expect("explicit auth redirect");
            assert!(error
                .downcast_ref::<ProviderSessionExpiredError>()
                .is_some());
        }
    }

    #[test]
    fn final_response_snapshot_must_reject_auth_route_at_unchanged_old_text() {
        use crate::providers::common::ResponseProbe;
        let initial = ResponseProbe {
            url: "https://chatgpt.com/c/turn".to_owned(),
            text: Some("previous answer".to_owned()),
            count: 1,
            populated_count: 1,
            ..ResponseProbe::default()
        };
        let mut final_probe = initial.clone();
        final_probe.url = "https://chatgpt.com/auth/login".to_owned();
        // Text and counts can remain stable during an SPA auth transition.
        assert!(super::final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            initial.populated_count,
            &final_probe,
            super::DoneSignal::StableResponseText,
            false,
            false,
        ));
        let error = super::probe_login_redirect_error("chatgpt", &final_probe)
            .expect("a final first-party auth route must override stable stale text");
        assert!(error
            .downcast_ref::<ProviderSessionExpiredError>()
            .is_some());
        assert!(
            super::probe_login_redirect_error("chatgpt", &initial).is_none(),
            "normal final conversation sample must not be an auth redirect"
        );
        final_probe.url =
            "https://chatgpt.com/c/turn?next=https%3A%2F%2Faccounts.google.com%2F".into();
        assert!(super::probe_login_redirect_error("chatgpt", &final_probe).is_none());
        final_probe.url = "https://attacker.invalid/login".into();
        assert!(
            super::probe_login_redirect_error("chatgpt", &final_probe).is_none(),
            "unknown origin remains a redacted off-provider error, not expired auth"
        );
    }

    #[test]
    fn another_known_provider_login_never_reports_requested_session_expired() {
        for (expected, actual_url) in [
            ("chatgpt", "https://manus.im/auth/login"),
            ("manus", "https://chatgpt.com/auth/login"),
            ("claude", "https://www.meta.ai/login"),
            ("grok", "https://gemini.google.com/login"),
        ] {
            assert!(
                login_redirect_error(expected, actual_url).is_none(),
                "different provider login is unexpected navigation, not auth expiry"
            );
            let probe = crate::providers::common::ResponseProbe {
                url: actual_url.to_owned(),
                ..Default::default()
            };
            assert!(super::probe_login_redirect_error(expected, &probe).is_none());
        }
        assert!(login_redirect_error("manus", "https://manus.im/auth/login").is_some());
        assert!(login_redirect_error("chatgpt", "https://chatgpt.com/auth/login").is_some());
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
            assert!(
                login_redirect_error("chatgpt", url).is_none(),
                "false expiry: {url}"
            );
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
    initial_populated_count: usize,
    final_probe: &common::ResponseProbe,
    done_signal: DoneSignal,
    saw_streaming: bool,
    confirmed_post_answer_affordance: bool,
) -> bool {
    if final_probe.streaming
        || final_probe.text.as_deref() != Some(initial_text)
        || final_probe.count != initial_count
        || final_probe.populated_count != initial_populated_count
    {
        // A blank/newly populated root can change without changing the
        // total node count or the earlier turn's last nonempty text.
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

/// After any adapter accepts Send, a later redirect to an unrelated
/// origin must never be treated as a provider response or terminal state.
/// Use the complete nine-provider HTTPS origin policy from the Send path:
/// the strict submission-acknowledgment classifier recognizes only four.
fn require_expected_response_origin(provider: &str, page: &Page) -> anyhow::Result<()> {
    common::require_page_provider_origin(provider, page)
}

/// A page can navigate after the DOM probe captured assistant text. Check the
/// atomically captured probe URL as well as the current page URL; do not allow
/// an earlier foreign response to inherit a later permitted page location.
fn require_expected_response_snapshot_origin(
    provider: &str,
    page: &Page,
    snapshot_url: &str,
) -> anyhow::Result<()> {
    common::require_response_probe_origins(provider, page, snapshot_url)
}

/// A challenge-looking URL or overlay on a foreign page must not persistently
/// disable the requested provider. Only its own strict HTTPS page or a
/// narrowly known external verification endpoint may trip the sticky breaker.
/// The returned off-provider error contains no URL, prompt or page content.
fn untrusted_challenge_origin_error(provider: &str, page: &Page) -> Option<anyhow::Error> {
    if crate::provider_guard::is_known_external_challenge_url(&page.url()) {
        return None;
    }
    require_expected_response_origin(provider, page).err()
}

/// A polling DOM snapshot can precede a navigation to a different page.
/// Challenge evidence may persistently disable a provider only if *both*
/// URL observations belong to the expected provider or known HTTPS verifier.
/// Do not let an off-provider verification overlay borrow the later page URL.
fn challenge_probe_urls_attributable(
    provider: &str,
    current_url: &str,
    snapshot_url: &str,
) -> bool {
    common::require_response_origin_pair(provider, current_url, snapshot_url).is_ok()
        || (crate::provider_guard::is_known_external_challenge_url(current_url)
            && crate::provider_guard::is_known_external_challenge_url(snapshot_url))
}

fn untrusted_challenge_probe_origin_error(
    provider: &str,
    page: &Page,
    snapshot_url: &str,
) -> Option<anyhow::Error> {
    #[cfg(test)]
    if common::require_response_probe_origins(provider, page, snapshot_url).is_ok() {
        // Existing explicitly marked in-memory offline test fixtures only.
        return None;
    }
    if challenge_probe_urls_attributable(provider, &page.url(), snapshot_url) {
        None
    } else {
        Some(anyhow::anyhow!(
            "provider chat origin unavailable before prompt dispatch"
        ))
    }
}

#[cfg(test)]
mod response_origin_guard_tests {
    use super::require_expected_response_origin;

    #[tokio::test]
    async fn foreign_challenge_overlay_must_not_stick_to_unrelated_adapter() {
        use crate::provider_guard::ProviderGuard;

        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<html><body><div>Verify you are human</div></body></html>")
            .await
            .unwrap();
        let guard = ProviderGuard::default();
        assert!(crate::provider_guard::is_challenge_page(&page));
        for spec in crate::providers::ADAPTERS {
            let error = super::untrusted_challenge_origin_error(spec.id, &page)
                .expect("foreign challenge-looking page is not provider proof");
            assert!(
                error.to_string().contains("chat origin unavailable"),
                "{} unexpectedly trusted off-provider challenge",
                spec.id
            );
            assert_eq!(
                guard.provider_disabled_reason(spec.id),
                None,
                "a foreign challenge page cannot stick the provider breaker"
            );
        }

        // The offline fixture exemption is explicitly test-only. It does not
        // widen any production provider or external challenge origin.
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture', '')",
        );
        assert!(super::untrusted_challenge_origin_error("manus", &page).is_none());
    }

    #[test]
    fn challenge_sticky_attribution_requires_both_url_samples_to_be_trusted() {
        for spec in crate::providers::ADAPTERS {
            assert!(
                super::challenge_probe_urls_attributable(spec.id, spec.url, spec.url),
                "{} genuine challenge overlay on own site is attributable",
                spec.id
            );
            for bad_url in [
                "https://attacker.invalid/captcha",
                "https://www.manus.im.attacker.invalid/captcha",
                "http://manus.im/captcha",
                "https://user@manus.im/captcha",
                "https://manus.im:65536/captcha",
                "https://accounts.google.com.attacker.invalid/v3/signin/challenge",
            ] {
                assert!(
                    !super::challenge_probe_urls_attributable(spec.id, spec.url, bad_url),
                    "{} may not disable after an untrusted captured challenge snapshot",
                    spec.id
                );
                assert!(
                    !super::challenge_probe_urls_attributable(spec.id, bad_url, spec.url),
                    "{} may not disable after an untrusted current URL",
                    spec.id
                );
            }
        }
        let provider = "manus";
        for verifier in [
            "https://challenges.cloudflare.com/captcha",
            "https://challenge.cloudflare.com/",
            "https://accounts.google.com/v3/signin/challenge",
        ] {
            assert!(
                super::challenge_probe_urls_attributable(provider, verifier, verifier),
                "known external HTTPS verifier remains attributable"
            );
            assert!(
                !super::challenge_probe_urls_attributable(
                    provider,
                    verifier,
                    "https://attacker.invalid/captcha"
                ),
                "a known verifier cannot bless an arbitrary captured URL"
            );
            assert!(
                !super::challenge_probe_urls_attributable(provider, verifier, "https://manus.im/"),
                "inconsistent verifier/first-party samples are not sticky proof"
            );
        }
    }

    #[tokio::test]
    async fn foreign_challenge_cannot_disable_manus_at_request_entry() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = "<html><body><div role='dialog'>Verify you are human</div></body></html>";
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        assert!(crate::provider_guard::is_challenge_page(&page));

        let guard = crate::provider_guard::ProviderGuard::default();
        let cfg: crate::config::ProviderConfig = serde_yaml::from_str("name: manus").unwrap();
        let adapter: std::sync::Arc<dyn super::SiteAdapter> =
            std::sync::Arc::new(crate::providers::manus::ManusAdapter { cfg });
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        super::run_send(
            adapter,
            &mut page,
            "never dispatch this prompt".to_owned(),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(50),
            super::ProviderSendPolicy {
                completion_idle: std::time::Duration::from_millis(150),
                provider_guard: guard.clone(),
                metrics: crate::metrics::MetricsRegistry::default(),
            },
            sender,
        )
        .await;
        let error = receiver.recv().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("chat origin unavailable"));
        assert_eq!(guard.provider_disabled_reason("manus"), None);
        assert!(
            receiver.try_recv().is_err(),
            "no assistant content may escape"
        );
    }

    #[tokio::test]
    async fn response_snapshot_origin_uses_explicit_offline_fixture_only_in_tests() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = "<html><body><div class='answer'>old reply</div></body></html>";
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        for spec in crate::providers::ADAPTERS {
            assert!(
                super::require_expected_response_snapshot_origin(
                    spec.id,
                    &page,
                    "https://unrelated.invalid/c/old"
                )
                .is_err(),
                "{} must not trust a foreign data page just because it has a reply selector",
                spec.id
            );
        }
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture', '')",
        );
        for spec in crate::providers::ADAPTERS {
            assert!(
                super::require_expected_response_snapshot_origin(
                    spec.id,
                    &page,
                    "data:text/html,offline"
                )
                .is_ok(),
                "{} must preserve explicitly tagged offline test fixture access",
                spec.id
            );
        }
    }

    #[tokio::test]
    async fn foreign_page_with_response_nodes_cannot_complete_strict_provider() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto(
            "data:text/html,<html><body><div class='assistant-message'>unrelated content</div></body></html>",
        )
        .await
        .unwrap();
        // All nine adapters must refuse foreign-page assistant text, not
        // merely the ChatGPT/Grok paths that were guarded originally.
        for provider in [
            "chatgpt",
            "grok",
            "claude",
            "gemini",
            "qwen",
            "metaai",
            "manus",
            "mistral",
            "perplexity",
        ] {
            let error = require_expected_response_origin(provider, &page).unwrap_err();
            assert!(
                error.to_string().contains("chat origin unavailable"),
                "{provider}"
            );
            assert!(!error.to_string().contains("unrelated content"));
        }

        // Keep the explicit synthetic fixture exception restricted to tests.
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture', '')",
        );
        for provider in [
            "chatgpt",
            "grok",
            "claude",
            "gemini",
            "qwen",
            "metaai",
            "manus",
            "mistral",
            "perplexity",
        ] {
            assert!(
                require_expected_response_origin(provider, &page).is_ok(),
                "{provider}"
            );
        }
    }
}

#[cfg(test)]
mod baseline_provenance_before_send_tests {
    use super::{AdapterSelectors, ProviderSendPolicy, SiteAdapter};
    use crate::providers::common::{self, EffectiveSelectorSet, ResponseProbe, SelectorSet};
    use anyhow::Result;
    use obscura_core::Page;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    struct BaselineSpyAdapter {
        cfg: crate::config::ProviderConfig,
        sends: Arc<AtomicUsize>,
        forced_probe: Option<ResponseProbe>,
    }

    #[async_trait::async_trait(?Send)]
    impl SiteAdapter for BaselineSpyAdapter {
        fn name(&self) -> &str {
            "manus"
        }
        fn aliases(&self) -> &[String] {
            &self.cfg.models
        }
        async fn discover_models(&self, _page: &mut Page) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        async fn send_prompt(&self, _page: &mut Page, _prompt: &str) -> Result<()> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn response_text(&self, _page: &Page) -> Option<String> {
            None
        }
        fn response_probe(&self, page: &Page, challenge: bool) -> ResponseProbe {
            self.forced_probe
                .clone()
                .unwrap_or_else(|| common::response_probe(page, &[".answer"], challenge))
        }
        fn selectors(&self) -> AdapterSelectors {
            self.effective_selectors().adapter_selectors()
        }
        fn selector_set(&self) -> &SelectorSet {
            &crate::providers::manus::SELECTORS
        }
        fn effective_selectors(&self) -> EffectiveSelectorSet {
            crate::providers::manus::SELECTORS.effective(&self.cfg.selectors)
        }
    }

    async fn exercise_baseline(
        html: &str,
        forced_probe: Option<ResponseProbe>,
    ) -> (anyhow::Error, usize, Option<&'static str>) {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let sends = Arc::new(AtomicUsize::new(0));
        let cfg = serde_yaml::from_str("name: manus").unwrap();
        let guard = crate::provider_guard::ProviderGuard::default();
        let adapter: Arc<dyn SiteAdapter> = Arc::new(BaselineSpyAdapter {
            cfg,
            sends: Arc::clone(&sends),
            forced_probe,
        });
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        super::run_send(
            adapter,
            &mut page,
            "private prompt must never leave foreign baseline".to_owned(),
            Duration::from_millis(75),
            Duration::from_millis(20),
            ProviderSendPolicy {
                completion_idle: Duration::from_millis(75),
                provider_guard: guard.clone(),
                metrics: crate::metrics::MetricsRegistry::default(),
            },
            sender,
        )
        .await;
        let error = receiver
            .recv()
            .await
            .expect("a rejection must be emitted")
            .expect_err("untrusted baseline or bounded test timeout cannot succeed");
        assert!(
            receiver.try_recv().is_err(),
            "baseline failure cannot be followed by assistant output"
        );
        (
            error,
            sends.load(Ordering::SeqCst),
            guard.provider_disabled_reason("manus"),
        )
    }

    #[tokio::test]
    async fn foreign_baseline_refuses_to_dispatch_prompt_even_with_visible_reply() {
        let html = "<html><body><div class='answer'>foreign old answer</div></body></html>";
        let (error, sends, disabled) = exercise_baseline(html, None).await;
        assert!(error.to_string().contains("chat origin unavailable"));
        assert!(!error.to_string().contains("foreign old answer"));
        assert!(!error.to_string().contains("private prompt"));
        assert_eq!(sends, 0, "no prompt may reach a foreign baseline adapter");
        assert_eq!(disabled, None);
    }

    #[tokio::test]
    async fn untrusted_captcha_baseline_cannot_stick_manus_breaker_or_send() {
        let html = "<html><body><div class='answer'>foreign content</div></body></html>";
        let probe = ResponseProbe {
            url: "https://attacker.invalid/captcha".into(),
            challenge: true,
            text: Some("Verify you are human".into()),
            ..ResponseProbe::default()
        };
        let (error, sends, disabled) = exercise_baseline(html, Some(probe)).await;
        assert!(error.to_string().contains("chat origin unavailable"));
        assert_eq!(sends, 0);
        assert_eq!(disabled, None);
    }

    #[tokio::test]
    async fn explicit_test_only_fixture_still_reaches_the_spy_adapter() {
        let html = "<html data-chat-gateway-test-fixture><body><div class='answer'>previous</div></body></html>";
        let (error, sends, disabled) = exercise_baseline(html, None).await;
        assert_eq!(
            sends, 1,
            "an explicit in-memory fixture may reach mock Send"
        );
        assert!(
            error
                .downcast_ref::<common::ProviderTimeoutError>()
                .is_some(),
            "a mock Send without a reply must finish by typed timeout"
        );
        assert_eq!(disabled, None);
    }
}

#[cfg(test)]
mod closed_consumer_before_dispatch_tests {
    use super::{AdapterSelectors, ProviderChunk, ProviderSendPolicy, SiteAdapter};
    use crate::providers::common::{self, EffectiveSelectorSet, ResponseProbe, SelectorSet};
    use anyhow::Result;
    use obscura_core::Page;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use std::time::Duration;
    use tokio::sync::mpsc;

    type HeldReceiver = Arc<Mutex<Option<mpsc::Receiver<Result<ProviderChunk>>>>>;

    struct DisconnectSpy {
        cfg: crate::config::ProviderConfig,
        sends: Arc<AtomicUsize>,
        drop_receiver_on_baseline: Option<HeldReceiver>,
    }

    #[async_trait::async_trait(?Send)]
    impl SiteAdapter for DisconnectSpy {
        fn name(&self) -> &str {
            "qwen"
        }

        fn aliases(&self) -> &[String] {
            &self.cfg.models
        }

        async fn discover_models(&self, _page: &mut Page) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn send_prompt(&self, _page: &mut Page, _prompt: &str) -> Result<()> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::Error::new(
                common::ProviderSubmissionUnacknowledgedError,
            ))
        }

        fn response_text(&self, _page: &Page) -> Option<String> {
            None
        }

        fn response_probe(&self, page: &Page, challenge: bool) -> ResponseProbe {
            if let Some(receiver) = &self.drop_receiver_on_baseline {
                let _ = receiver.lock().unwrap().take();
            }
            common::response_probe(page, &[".answer"], challenge)
        }

        fn selectors(&self) -> AdapterSelectors {
            self.effective_selectors().adapter_selectors()
        }

        fn selector_set(&self) -> &SelectorSet {
            &crate::providers::qwen::SELECTORS
        }

        fn effective_selectors(&self) -> EffectiveSelectorSet {
            crate::providers::qwen::SELECTORS.effective(&self.cfg.selectors)
        }
    }

    async fn test_page(browser: &obscura_core::Browser) -> Page {
        let mut page = browser.new_page().await.unwrap();
        let html = "<html data-chat-gateway-test-fixture><body><div class='answer'>old response</div></body></html>";
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        page
    }

    fn spy(
        sends: &Arc<AtomicUsize>,
        drop_receiver_on_baseline: Option<HeldReceiver>,
    ) -> Arc<dyn SiteAdapter> {
        Arc::new(DisconnectSpy {
            cfg: serde_yaml::from_str("name: qwen").unwrap(),
            sends: Arc::clone(sends),
            drop_receiver_on_baseline,
        })
    }

    async fn send(
        adapter: Arc<dyn SiteAdapter>,
        page: &mut Page,
        sender: mpsc::Sender<Result<ProviderChunk>>,
    ) {
        super::run_send(
            adapter,
            page,
            "private test prompt".to_owned(),
            Duration::from_millis(80),
            Duration::from_millis(20),
            ProviderSendPolicy {
                completion_idle: Duration::from_millis(80),
                provider_guard: crate::provider_guard::ProviderGuard::default(),
                metrics: crate::metrics::MetricsRegistry::default(),
            },
            sender,
        )
        .await;
    }

    #[tokio::test]
    async fn receiver_closed_at_entry_never_dispatches_a_prompt() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = test_page(&browser).await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = mpsc::channel(8);
        drop(receiver);

        send(spy(&attempts, None), &mut page, sender).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn receiver_closed_during_baseline_never_reaches_send_prompt() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = test_page(&browser).await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = mpsc::channel(8);
        let holder = Arc::new(Mutex::new(Some(receiver)));

        send(spy(&attempts, Some(Arc::clone(&holder))), &mut page, sender).await;
        assert!(holder.lock().unwrap().is_none());
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn live_consumer_still_reaches_adapter_and_receives_typed_error() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = test_page(&browser).await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let (sender, mut receiver) = mpsc::channel(8);

        send(spy(&attempts, None), &mut page, sender).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let err = receiver
            .recv()
            .await
            .expect("live receiver should obtain adapter result")
            .expect_err("mock adapter intentionally rejects unacknowledged Send");
        assert!(err
            .downcast_ref::<common::ProviderSubmissionUnacknowledgedError>()
            .is_some());
    }
}

#[cfg(test)]
mod submission_timeout_cancellation_tests {
    use super::{AdapterSelectors, ProviderSendPolicy, SiteAdapter};
    use crate::providers::common::{self, EffectiveSelectorSet, SelectorSet};
    use anyhow::Result;
    use obscura_core::Page;
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Clone, Copy)]
    enum MockSubmissionOutcome {
        Timeout,
        Unacknowledged,
        PreSendInputRejected,
    }

    struct HangingSubmissionAdapter {
        cfg: crate::config::ProviderConfig,
        revoke_fixture_before_timeout: bool,
        outcome: MockSubmissionOutcome,
    }

    #[async_trait::async_trait(?Send)]
    impl SiteAdapter for HangingSubmissionAdapter {
        fn name(&self) -> &str {
            "qwen"
        }

        fn aliases(&self) -> &[String] {
            &self.cfg.models
        }

        async fn discover_models(&self, _page: &mut Page) -> Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn send_prompt(&self, page: &mut Page, _prompt: &str) -> Result<()> {
            if matches!(self.outcome, MockSubmissionOutcome::PreSendInputRejected) {
                return Err(anyhow::Error::new(
                    common::ProviderComposerInputRejectedError,
                ));
            }
            page.evaluate("window.sendAttempts = (window.sendAttempts || 0) + 1");
            if self.revoke_fixture_before_timeout {
                // Simulate navigation to an untrusted origin without relying
                // on external network or a live authenticated provider.
                page.evaluate(
                    "document.documentElement.removeAttribute('data-chat-gateway-test-fixture')",
                );
            }
            if matches!(self.outcome, MockSubmissionOutcome::Unacknowledged) {
                return Err(anyhow::Error::new(
                    common::ProviderSubmissionUnacknowledgedError,
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(())
        }

        fn response_text(&self, _page: &Page) -> Option<String> {
            None
        }

        fn selectors(&self) -> AdapterSelectors {
            self.effective_selectors().adapter_selectors()
        }

        fn selector_set(&self) -> &SelectorSet {
            &crate::providers::qwen::SELECTORS
        }

        fn effective_selectors(&self) -> EffectiveSelectorSet {
            crate::providers::qwen::SELECTORS.effective(&self.cfg.selectors)
        }
    }

    async fn exercise_submission(
        stop_markup: &str,
        revoke_fixture: bool,
        outcome: MockSubmissionOutcome,
    ) -> (f64, f64) {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html =
            format!("<html data-chat-gateway-test-fixture><body>{stop_markup}</body></html>");
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let adapter: Arc<dyn SiteAdapter> = Arc::new(HangingSubmissionAdapter {
            cfg: serde_yaml::from_str("name: qwen").unwrap(),
            revoke_fixture_before_timeout: revoke_fixture,
            outcome,
        });
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        super::run_send(
            adapter,
            &mut page,
            "private prompt used only for this offline test".to_owned(),
            Duration::from_millis(40),
            Duration::from_millis(10),
            ProviderSendPolicy {
                completion_idle: Duration::from_millis(40),
                provider_guard: crate::provider_guard::ProviderGuard::default(),
                metrics: crate::metrics::MetricsRegistry::default(),
            },
            sender,
        )
        .await;
        let error = receiver
            .recv()
            .await
            .expect("submission must emit a terminal typed error")
            .expect_err("a rejected or timed-out submission must not succeed");
        match outcome {
            MockSubmissionOutcome::Unacknowledged => {
                assert!(
                    error
                        .downcast_ref::<common::ProviderSubmissionUnacknowledgedError>()
                        .is_some(),
                    "a rejected submission must preserve its downcastable error"
                );
            }
            MockSubmissionOutcome::PreSendInputRejected => {
                assert!(
                    error
                        .downcast_ref::<common::ProviderComposerInputRejectedError>()
                        .is_some(),
                    "an input failure must preserve its typed error"
                );
            }
            MockSubmissionOutcome::Timeout => {
                assert!(
                    error
                        .downcast_ref::<common::ProviderTimeoutError>()
                        .is_some(),
                    "submission timeout remains downcastable"
                );
                assert!(error.to_string().contains("timed out while submitting"));
            }
        }
        assert!(!error.to_string().contains("private prompt"));
        assert!(
            receiver.try_recv().is_err(),
            "timed-out submission must not emit assistant success"
        );
        (
            page.evaluate("window.sendAttempts || 0")
                .as_f64()
                .unwrap_or(0.0),
            page.evaluate("window.cancelClicks || 0")
                .as_f64()
                .unwrap_or(0.0),
        )
    }

    #[tokio::test]
    async fn stalled_submission_requests_stop_before_warm_page_reuse() {
        let stop = r#"<button aria-label="Stop generating" type="button"
            onclick="window.cancelClicks=(window.cancelClicks||0)+1">Stop</button>"#;
        let (attempts, cancels) =
            exercise_submission(stop, false, MockSubmissionOutcome::Timeout).await;
        assert_eq!(attempts, 1.0);
        assert_eq!(cancels, 1.0, "cancel one genuine in-flight generation");
    }

    #[tokio::test]
    async fn timeout_never_clicks_unrelated_stop_or_foreign_page_control() {
        let unrelated = r#"<button type="button" aria-label="Stop sharing"
            onclick="window.cancelClicks=(window.cancelClicks||0)+1">Stop sharing</button>"#;
        let (attempts, cancels) =
            exercise_submission(unrelated, false, MockSubmissionOutcome::Timeout).await;
        assert_eq!(attempts, 1.0);
        assert_eq!(cancels, 0.0);

        let foreign = r#"<button aria-label="Stop generating" type="button"
            onclick="window.cancelClicks=(window.cancelClicks||0)+1">Stop</button>"#;
        let (attempts, cancels) =
            exercise_submission(foreign, true, MockSubmissionOutcome::Timeout).await;
        assert_eq!(attempts, 1.0);
        assert_eq!(
            cancels, 0.0,
            "lost provider origin must block cancellation side effects"
        );
    }

    #[tokio::test]
    async fn rejected_submission_cancels_possible_generation_without_losing_typed_error() {
        let stop = r#"<button aria-label="Stop generating" type="button"
            onclick="window.cancelClicks=(window.cancelClicks||0)+1">Stop</button>"#;
        let (attempts, cancels) =
            exercise_submission(stop, false, MockSubmissionOutcome::Unacknowledged).await;
        assert_eq!(attempts, 1.0);
        assert_eq!(
            cancels, 1.0,
            "a rejected acknowledgement may follow a real Send click"
        );

        let (attempts, cancels) =
            exercise_submission(stop, true, MockSubmissionOutcome::Unacknowledged).await;
        assert_eq!(attempts, 1.0);
        assert_eq!(
            cancels, 0.0,
            "a foreign or untrusted page must never receive a cancellation click"
        );
    }

    #[tokio::test]
    async fn input_rejected_before_send_does_not_cancel_inherited_generation() {
        let stop = r#"<button aria-label="Stop generating" type="button"
            onclick="window.cancelClicks=(window.cancelClicks||0)+1">Stop</button>"#;
        let (attempts, cancels) =
            exercise_submission(stop, false, MockSubmissionOutcome::PreSendInputRejected).await;
        assert_eq!(
            attempts, 0.0,
            "no Send was attempted before input rejection"
        );
        assert_eq!(
            cancels, 0.0,
            "pre-Send errors must not click an inherited Stop control"
        );
    }

    #[tokio::test]
    async fn rejected_submission_does_not_click_unrelated_stop_sharing() {
        let unrelated = r#"<button type="button" aria-label="Stop sharing"
            onclick="window.cancelClicks=(window.cancelClicks||0)+1">Stop sharing</button>"#;
        let (attempts, cancels) =
            exercise_submission(unrelated, false, MockSubmissionOutcome::Unacknowledged).await;
        assert_eq!(attempts, 1.0);
        assert_eq!(cancels, 0.0);
    }
}

/// A stale or redirected page may expose an unrelated visible "Stop".
/// Never click that page while cleaning up the previous provider request.
/// This is a best-effort current-origin fence; actual first-party browser
/// cancellation and cross-navigation atomicity still need live evidence.
fn cancel_generation_if_trusted(adapter: &dyn SiteAdapter, page: &Page) {
    if login_redirect_error(adapter.name(), &page.url()).is_none()
        && require_expected_response_origin(adapter.name(), page).is_ok()
        && !crate::provider_guard::is_challenge_page(page)
    {
        adapter.cancel_generation(page);
    }
}

#[cfg(test)]
mod post_send_cancel_origin_tests {
    use super::{cancel_generation_if_trusted, qwen};
    use obscura_core::Page;

    async fn fixture() -> (obscura_core::Browser, Page) {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html data-chat-gateway-test-fixture><body>
            <button aria-label="Stop generating" id="active-stop"
                onclick="window.stopCount=(window.stopCount||0)+1">Stop</button>
            <button aria-label="Stop sharing"
                onclick="window.unrelatedCount=(window.unrelatedCount||0)+1">Stop sharing</button>
        </body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        (browser, page)
    }

    #[tokio::test]
    async fn trusted_post_send_cancellation_clicks_only_real_stop() {
        let (_browser, page) = fixture().await;
        let adapter = qwen::QwenAdapter {
            cfg: serde_yaml::from_str("name: qwen").unwrap(),
        };
        cancel_generation_if_trusted(&adapter, &page);
        assert_eq!(page.evaluate("window.stopCount || 0").as_f64(), Some(1.0));
        assert_eq!(
            page.evaluate("window.unrelatedCount || 0").as_f64(),
            Some(0.0)
        );

        page.evaluate("document.querySelector('#active-stop').disabled = true");
        cancel_generation_if_trusted(&adapter, &page);
        assert_eq!(
            page.evaluate("window.stopCount || 0").as_f64(),
            Some(1.0),
            "disabled Stop clones cannot be clicked"
        );
    }

    #[tokio::test]
    async fn cancellation_skips_lost_origin_and_challenge_page() {
        let (_browser, page) = fixture().await;
        let adapter = qwen::QwenAdapter {
            cfg: serde_yaml::from_str("name: qwen").unwrap(),
        };
        page.evaluate("document.documentElement.removeAttribute('data-chat-gateway-test-fixture')");
        cancel_generation_if_trusted(&adapter, &page);
        assert_eq!(
            page.evaluate("window.stopCount || 0").as_f64(),
            Some(0.0),
            "off-provider DOM must not receive a Stop click"
        );

        page.evaluate("document.documentElement.setAttribute('data-chat-gateway-test-fixture','')");
        page.evaluate(
            "document.body.insertAdjacentHTML('beforeend','<h1>Security challenge</h1>')",
        );
        cancel_generation_if_trusted(&adapter, &page);
        assert_eq!(
            page.evaluate("window.stopCount || 0").as_f64(),
            Some(0.0),
            "verification UI must not receive unrelated cleanup clicks"
        );
    }
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
    // A disconnected HTTP consumer must never cause a new provider Send.
    // The later polling-loop check cannot undo a prompt already dispatched.
    if chunks.is_closed() {
        return;
    }
    if crate::provider_guard::is_challenge_page(page) {
        if let Some(error) = untrusted_challenge_origin_error(adapter.name(), page) {
            let _ = chunks.send(Err(error)).await;
            return;
        }
        provider_guard.disable_provider(
            adapter.name(),
            "challenge page detected; manual review required",
        );
        let _ = chunks
            .send(Err(provider_challenge_error(adapter.name())))
            .await;
        return;
    }
    if let Some(error) = login_redirect_error(adapter.name(), &page.url()) {
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
    // The assistant baseline is part of the security-sensitive provenance
    // for this request. Verify the same-DOM-sample URL and the current page
    // before trusting prior reply text or handing the new prompt to an adapter.
    // Navigation may have occurred since the earlier entry guard.
    if probe_has_challenge(&baseline_probe) {
        if let Some(error) =
            untrusted_challenge_probe_origin_error(adapter.name(), page, &baseline_probe.url)
        {
            let _ = chunks.send(Err(error)).await;
            return;
        }
        provider_guard.disable_provider(
            adapter.name(),
            "challenge page detected; manual review required",
        );
        let _ = chunks
            .send(Err(provider_challenge_error(adapter.name())))
            .await;
        return;
    }
    if let Some(auth_error) = probe_login_redirect_error(adapter.name(), &baseline_probe) {
        let _ = chunks.send(Err(auth_error)).await;
        return;
    }
    if let Err(error) =
        require_expected_response_snapshot_origin(adapter.name(), page, &baseline_probe.url)
    {
        let _ = chunks.send(Err(error)).await;
        return;
    }
    let baseline_response_count = baseline_probe.count;
    let baseline_populated_count = baseline_probe.populated_count;
    let baseline = baseline_probe.text.filter(|t| !t.is_empty());
    let identical_answer_fallback = baseline.clone();
    let mut baseline = baseline;

    // Apply the same timeout budget to prompt submission and response
    // polling; otherwise a stuck selector/input action can hang indefinitely
    // before the existing polling timeout even starts.
    // The receiver can disappear during the synchronous baseline probe.
    // Recheck at the final handoff before any provider-side input mutation.
    if chunks.is_closed() {
        return;
    }
    let start = Instant::now();
    match tokio::time::timeout(timeout, adapter.send_prompt(page, &prompt)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            // A provider can redirect to manual verification while the
            // composer is being typed or the Send control is settling.
            // Preserve the typed challenge category rather than exposing
            // a misleading missing-composer or submission error.
            if crate::provider_guard::is_challenge_page(page) {
                if let Some(error) = untrusted_challenge_origin_error(adapter.name(), page) {
                    let _ = chunks.send(Err(error)).await;
                    return;
                }
                provider_guard.disable_provider(
                    adapter.name(),
                    "challenge page detected; manual review required",
                );
                let _ = chunks
                    .send(Err(provider_challenge_error(adapter.name())))
                    .await;
            } else if let Some(auth_error) = login_redirect_error(adapter.name(), &page.url()) {
                let _ = chunks.send(Err(auth_error)).await;
            } else {
                // A typed failed acknowledgement can follow an actual Send
                // click. The remote model may still be generating. Preserve
                // the original typed error, but first request best-effort
                // cancellation only while the page still belongs to this
                // provider. Do not interact with unrelated redirected pages.
                if error.is::<common::ProviderSubmissionUnacknowledgedError>()
                    && require_expected_response_origin(adapter.name(), page).is_ok()
                {
                    adapter.cancel_generation(page);
                }
                let _ = chunks.send(Err(error)).await;
            }
            return;
        }
        Err(_) => {
            metrics.record_provider_timeout(adapter.name(), "submission");
            // Send may already have been clicked before a framework ack hung.
            // Best-effort cancel so a warm page cannot retain that generation.
            // Never click a Stop control after navigation to an off-provider
            // page or authentication redirect, and keep the original timeout.
            if login_redirect_error(adapter.name(), &page.url()).is_none()
                && require_expected_response_origin(adapter.name(), page).is_ok()
            {
                adapter.cancel_generation(page);
            }
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
    let mut citation_filter = citation_re.as_ref().map(|_| CitationDeltaFilter::new());
    let mut tracker = ResponseDeltaTracker::new(baseline.take());
    let mut stable_ticks = 0u32;
    let mut last_delta_at: Option<Instant> = None;
    let mut fresh_stop = FreshStopSignal::new(baseline_probe.streaming);
    let mut previous_post_answer_affordance = false;
    let mut saw_post_answer_affordance_absent = !baseline_probe.post_answer_affordance;
    let mut response_node_detected = false;
    let mut saw_unpopulated_placeholder = false;
    let mut poll_count = 0u32;

    loop {
        if chunks.is_closed() {
            cancel_generation_if_trusted(adapter.as_ref(), page);
            tracing::debug!(
                provider = adapter.name(),
                "client disconnected; requested generation stop"
            );
            return;
        }

        if start.elapsed() > timeout {
            // Distinguish a send that never yielded fresh assistant content
            // from an answer whose partial text arrived but never completed.
            // Neither path is successful; preserving the typed failure prevents
            // committing stale or incomplete assistant turns.
            let phase = common::ProviderTimeoutPhase::after_submission(last_delta_at.is_some());
            metrics.record_provider_timeout(adapter.name(), phase.as_str());
            // The request is terminal, but the remote composer may still be
            // generating. Stop it before another send reuses this warm page.
            // Cancellation is best effort; the timeout remains an error.
            cancel_generation_if_trusted(adapter.as_ref(), page);
            let _ = chunks
                .send(Err(anyhow::Error::new(common::ProviderTimeoutError::new(
                    adapter.name(),
                    timeout,
                    phase,
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
            if let Some(error) =
                untrusted_challenge_probe_origin_error(adapter.name(), page, &probe.url)
            {
                let _ = chunks.send(Err(error)).await;
                return;
            }
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
        if let Some(auth_error) = probe_login_redirect_error(adapter.name(), &probe) {
            let _ = chunks.send(Err(auth_error)).await;
            return;
        }
        if let Err(error) =
            require_expected_response_snapshot_origin(adapter.name(), page, &probe.url)
        {
            let _ = chunks.send(Err(error)).await;
            return;
        }
        // Observe Stop visibility even while a new blank reply placeholder
        // is pending; the placeholder itself must not reset/stream old text.
        fresh_stop.observe(probe.streaming);
        if !response_node_detected
            && has_unpopulated_new_response_placeholder(
                baseline_response_count,
                probe.count,
                baseline_populated_count,
                probe.populated_count,
            )
        {
            // The probe still returns the old populated answer while a new
            // blank placeholder is visible. Neither reset/stream the old
            // answer nor grant it terminal idle or Copy-control progress.
            saw_unpopulated_placeholder = true;
            stable_ticks = 0;
            previous_post_answer_affordance = false;
            continue;
        }
        if should_probe_response_count(response_node_detected)
            && should_reset_response_tracker(
                response_node_detected,
                baseline_response_count,
                probe.count,
                baseline_populated_count,
                probe.populated_count,
            )
        {
            // A *populated* new DOM node proves the response root changed,
            // even when its text repeats the prior answer verbatim. Latch
            // once; resetting on every poll would repeat the full delta.
            response_node_detected = true;
            tracker = ResponseDeltaTracker::new(None);
        }
        if stale_baseline_after_disappearing_placeholder(
            saw_unpopulated_placeholder,
            response_node_detected,
            identical_answer_fallback.as_deref(),
            probe.text.as_deref(),
        ) {
            // A canceled/rerendered empty new root is not evidence that the
            // previous answer was generated again. Wait for new content or
            // fail with the existing typed first-content timeout.
            stable_ticks = 0;
            previous_post_answer_affordance = false;
            continue;
        }

        let streaming = probe.streaming;
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
                    if fresh_stop.saw_fresh_generation() && !streaming {
                        stable_ticks += 1;
                    } else {
                        stable_ticks = 0;
                        continue;
                    }
                    if stable_ticks
                        >= completion_idle_ticks(
                            done_signal,
                            fresh_stop.saw_fresh_generation(),
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
                            if let Some(error) = untrusted_challenge_probe_origin_error(
                                adapter.name(),
                                page,
                                &final_probe.url,
                            ) {
                                let _ = chunks.send(Err(error)).await;
                                return;
                            }
                            provider_guard.disable_provider(
                                adapter.name(),
                                "challenge page detected; manual review required",
                            );
                            let _ = chunks
                                .send(Err(provider_challenge_error(adapter.name())))
                                .await;
                            return;
                        }
                        if let Some(auth_error) =
                            probe_login_redirect_error(adapter.name(), &final_probe)
                        {
                            let _ = chunks.send(Err(auth_error)).await;
                            return;
                        }
                        if let Err(error) = require_expected_response_snapshot_origin(
                            adapter.name(),
                            page,
                            &final_probe.url,
                        ) {
                            let _ = chunks.send(Err(error)).await;
                            return;
                        }
                        if !final_probe_confirms_completion(
                            &txt,
                            probe.count,
                            probe.populated_count,
                            &final_probe,
                            done_signal,
                            fresh_stop.saw_fresh_generation(),
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
                                cancel_generation_if_trusted(adapter.as_ref(), page);
                                return;
                            }
                        }
                        if let Some(last_delta_at) = last_delta_at {
                            metrics.record_provider_tail(
                                adapter.name(),
                                last_delta_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                            );
                        }
                        if let Some(filter) = citation_filter.as_mut() {
                            let suffix = filter.finish();
                            if !suffix.is_empty()
                                && chunks
                                    .send(Ok(ProviderChunk {
                                        content: suffix,
                                        done: false,
                                    }))
                                    .await
                                    .is_err()
                            {
                                cancel_generation_if_trusted(adapter.as_ref(), page);
                                return;
                            }
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
                    if let (Some(re), Some(filter)) =
                        (citation_re.as_ref(), citation_filter.as_mut())
                    {
                        delta = filter.push(&delta, re);
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
                        cancel_generation_if_trusted(adapter.as_ref(), page);
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
                fresh_stop.saw_fresh_generation(),
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
                    if let Some(error) = untrusted_challenge_probe_origin_error(
                        adapter.name(),
                        page,
                        &final_probe.url,
                    ) {
                        let _ = chunks.send(Err(error)).await;
                        return;
                    }
                    provider_guard.disable_provider(
                        adapter.name(),
                        "challenge page detected; manual review required",
                    );
                    let _ = chunks
                        .send(Err(provider_challenge_error(adapter.name())))
                        .await;
                    return;
                }
                if let Some(auth_error) = probe_login_redirect_error(adapter.name(), &final_probe) {
                    let _ = chunks.send(Err(auth_error)).await;
                    return;
                }
                if let Err(error) = require_expected_response_snapshot_origin(
                    adapter.name(),
                    page,
                    &final_probe.url,
                ) {
                    let _ = chunks.send(Err(error)).await;
                    return;
                }
                if !final_probe_confirms_completion(
                    &txt,
                    probe.count,
                    probe.populated_count,
                    &final_probe,
                    done_signal,
                    fresh_stop.saw_fresh_generation(),
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
                if let Some(filter) = citation_filter.as_mut() {
                    let suffix = filter.finish();
                    if !suffix.is_empty()
                        && chunks
                            .send(Ok(ProviderChunk {
                                content: suffix,
                                done: false,
                            }))
                            .await
                            .is_err()
                    {
                        cancel_generation_if_trusted(adapter.as_ref(), page);
                        return;
                    }
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
    fn identical_answer_requires_fresh_post_baseline_stop_edge() {
        let mut idle_baseline = FreshStopSignal::new(false);
        assert!(!idle_baseline.saw_fresh_generation());
        idle_baseline.observe(false);
        assert!(!idle_baseline.saw_fresh_generation());
        idle_baseline.observe(true);
        assert!(idle_baseline.saw_fresh_generation());
        idle_baseline.observe(false);
        assert!(idle_baseline.saw_fresh_generation());

        let mut inherited_stop = FreshStopSignal::new(true);
        inherited_stop.observe(true);
        inherited_stop.observe(true);
        inherited_stop.observe(false);
        assert!(
            !inherited_stop.saw_fresh_generation(),
            "the previous turn's Stop disappearing is not a new generation"
        );

        inherited_stop.observe(true);
        assert!(
            inherited_stop.saw_fresh_generation(),
            "an observed post-baseline inactive-to-active edge permits repeated text"
        );
        inherited_stop.observe(false);
        assert!(inherited_stop.saw_fresh_generation());

        let no_stop = FreshStopSignal::new(false);
        assert!(!no_stop.saw_fresh_generation());
    }

    #[test]
    fn inherited_stop_must_not_release_baseline_text_tracker() {
        let mut tracker = ResponseDeltaTracker::new(Some("previous answer".to_owned()));
        let mut stop = FreshStopSignal::new(true);
        stop.observe(true);
        stop.observe(false);
        assert_eq!(
            tracker.observe("previous answer"),
            DeltaObservation::WaitingForNewResponse
        );
        assert!(
            !stop.saw_fresh_generation(),
            "unchanged old text and inherited Stop cannot certify a new answer"
        );
        // A truly populated fresh root still uses its independent count
        // evidence even when the previous Stop was active.
        assert!(should_reset_response_tracker(false, 1, 2, 1, 2));
        tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("previous answer"),
            DeltaObservation::Delta("previous answer".to_owned())
        );
    }

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
        if should_reset_response_tracker(false, 3, 4, 3, 4) {
            tracker = ResponseDeltaTracker::new(None);
        }
        assert_eq!(
            tracker.observe("same answer"),
            DeltaObservation::Delta("same answer".to_string())
        );

        // The DOM node remains present on later polls, but must not reset the
        // tracker again or cause the same complete answer to be emitted twice.
        assert!(!should_reset_response_tracker(true, 3, 4, 3, 4));
        assert_eq!(tracker.observe("same answer"), DeltaObservation::Stable);
        assert!(!should_reset_response_tracker(false, 3, 3, 3, 3));
    }

    #[test]
    fn blank_new_placeholder_never_resets_or_completes_previous_reply() {
        let mut tracker = ResponseDeltaTracker::new(Some("old".to_owned()));
        assert!(has_unpopulated_new_response_placeholder(1, 2, 1, 1));
        assert!(!should_reset_response_tracker(false, 1, 2, 1, 1));
        assert_eq!(
            tracker.observe("old"),
            DeltaObservation::WaitingForNewResponse
        );

        // When the newly inserted response root is populated, the tracker
        // may reset and emit exactly one new answer.
        assert!(!has_unpopulated_new_response_placeholder(1, 2, 1, 2));
        assert!(should_reset_response_tracker(false, 1, 2, 1, 2));
        tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("fresh"),
            DeltaObservation::Delta("fresh".to_owned())
        );
        assert_eq!(tracker.observe("fresh"), DeltaObservation::Stable);
        assert!(!should_reset_response_tracker(true, 1, 2, 1, 2));

        // Repeated same text is legitimate ONLY after the new root actually
        // contains a response, not on the preceding empty placeholder.
        tracker = ResponseDeltaTracker::new(None);
        assert_eq!(
            tracker.observe("old"),
            DeltaObservation::Delta("old".to_owned())
        );
        assert!(!has_unpopulated_new_response_placeholder(1, 1, 1, 1));
        assert!(!should_reset_response_tracker(false, 1, 1, 1, 1));

        // A transient placeholder disappearing after Stop cannot certify
        // the old reply as a new identical answer.
        assert!(stale_baseline_after_disappearing_placeholder(
            true,
            false,
            Some("old"),
            Some("old")
        ));
        assert!(!stale_baseline_after_disappearing_placeholder(
            true,
            false,
            Some("old"),
            Some("fresh")
        ));
        assert!(!stale_baseline_after_disappearing_placeholder(
            true,
            true,
            Some("old"),
            Some("old")
        ));
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
    fn inherited_stop_never_unlocks_fast_done_without_fresh_generation_edge() {
        use super::FreshStopSignal;
        let done = DoneSignal::StopControlDisappeared;
        let full_grace = Duration::from_millis(15_000);

        let mut inherited = FreshStopSignal::new(true);
        inherited.observe(true);
        inherited.observe(false);
        assert!(
            !inherited.saw_fresh_generation(),
            "a Stop control present before Send cannot prove new generation"
        );
        assert_eq!(
            completion_idle_ticks(
                done,
                inherited.saw_fresh_generation(),
                true,
                DEFAULT_POLL_TICK,
                full_grace,
            ),
            COMPLETION_IDLE_TICKS,
        );
        assert!(!completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            false,
            done,
            inherited.saw_fresh_generation(),
            true,
            DEFAULT_POLL_TICK,
            full_grace,
        ));
        assert!(completion_ready(
            COMPLETION_IDLE_TICKS,
            false,
            done,
            inherited.saw_fresh_generation(),
            true,
            DEFAULT_POLL_TICK,
            full_grace,
        ));

        let mut fresh_after_inherited = FreshStopSignal::new(true);
        fresh_after_inherited.observe(false);
        fresh_after_inherited.observe(true);
        fresh_after_inherited.observe(false);
        assert!(fresh_after_inherited.saw_fresh_generation());
        assert!(completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            false,
            done,
            fresh_after_inherited.saw_fresh_generation(),
            true,
            DEFAULT_POLL_TICK,
            full_grace,
        ));

        let mut fresh_from_idle = FreshStopSignal::new(false);
        fresh_from_idle.observe(true);
        fresh_from_idle.observe(false);
        assert!(fresh_from_idle.saw_fresh_generation());
        assert!(completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            false,
            done,
            fresh_from_idle.saw_fresh_generation(),
            true,
            DEFAULT_POLL_TICK,
            full_grace,
        ));
        assert!(!completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            false,
            done,
            fresh_from_idle.saw_fresh_generation(),
            false,
            DEFAULT_POLL_TICK,
            full_grace,
        ));
        assert!(!completion_ready(
            EXPLICIT_DONE_IDLE_TICKS,
            false,
            DoneSignal::StableResponseText,
            fresh_from_idle.saw_fresh_generation(),
            true,
            DEFAULT_POLL_TICK,
            full_grace,
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
            populated_count: 2,
            streaming: false,
            post_answer_affordance: true,
            ..ResponseProbe::default()
        };
        let stable = initial.clone();
        assert!(final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            initial.populated_count,
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
            initial.populated_count,
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
            initial.populated_count,
            &new_node,
            DoneSignal::StableResponseText,
            false,
            false,
        ));

        // A root that becomes populated (or clears) while the total DOM
        // count and previous visible reply are unchanged invalidates final
        // completion; wait for new stable evidence instead.
        let mut newly_populated = stable.clone();
        newly_populated.populated_count += 1;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            initial.populated_count,
            &newly_populated,
            DoneSignal::StableResponseText,
            false,
            false,
        ));

        let mut cleared_root = stable.clone();
        cleared_root.populated_count -= 1;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            initial.populated_count,
            &cleared_root,
            DoneSignal::StableResponseText,
            false,
            false,
        ));

        let mut resumed = stable.clone();
        resumed.streaming = true;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            initial.populated_count,
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
            initial.populated_count,
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
            populated_count: 1,
            post_answer_affordance: true,
            ..ResponseProbe::default()
        };
        let mut final_probe = initial.clone();
        final_probe.post_answer_affordance = false;
        assert!(!final_probe_confirms_completion(
            initial.text.as_deref().unwrap(),
            initial.count,
            initial.populated_count,
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
            initial.populated_count,
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
    async fn inline_cancellation_refuses_unmarked_foreign_dom_even_with_real_stop() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body>
            <button aria-label="Stop generating"
                onclick="window.stopHits=(window.stopHits||0)+1">Stop</button>
        </body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let adapter = qwen::QwenAdapter {
            cfg: serde_yaml::from_str("name: qwen").unwrap(),
        };
        adapter.cancel_generation(&page);
        assert_eq!(
            page.evaluate("window.stopHits || 0").as_f64(),
            Some(0.0),
            "foreign page cannot borrow the Stop label to receive a click"
        );
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture','')",
        );
        adapter.cancel_generation(&page);
        assert_eq!(
            page.evaluate("window.stopHits || 0").as_f64(),
            Some(1.0),
            "only the explicitly marked offline test fixture may bypass HTTPS"
        );
        page.evaluate(
            "document.documentElement.removeAttribute('data-chat-gateway-test-fixture')",
        );
        adapter.cancel_generation(&page);
        assert_eq!(
            page.evaluate("window.stopHits || 0").as_f64(),
            Some(1.0),
            "removing the test-only marker must immediately restore rejection"
        );
    }

    #[tokio::test]
    async fn cancellation_skips_hidden_disabled_and_unrelated_stop_buttons() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html data-chat-gateway-test-fixture><body><button aria-label="Stop generating" style="display:none" onclick="window.hiddenClicks=(window.hiddenClicks||0)+1">Hidden</button><button aria-label="Stop sharing" onclick="window.unrelatedClicks=(window.unrelatedClicks||0)+1">Stop sharing</button><button aria-label="Stop generating" disabled onclick="window.disabledClicks=(window.disabledClicks||0)+1">Disabled</button><fieldset disabled><button aria-label="Stop generating" onclick="window.fieldsetClicks=(window.fieldsetClicks||0)+1">Disabled fieldset</button></fieldset><button aria-label="Stop generating" onclick="window.activeClicks=(window.activeClicks||0)+1">Stop</button></body></html>"#;
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

    #[tokio::test]
    async fn cancellation_ignores_opacity_zero_and_pointer_inert_stop_clones() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html data-chat-gateway-test-fixture><head><style>
            .transparent { opacity: 0; }
        </style></head><body>
            <button id="ghost" aria-label="Stop generating" class="transparent"
                onclick="window.ghostCount=(window.ghostCount||0)+1">Stop</button>
            <section class="transparent"><button id="ghost-child" aria-label="Stop generation"
                onclick="window.ghostChildCount=(window.ghostChildCount||0)+1">Stop</button></section>
            <button id="inert" aria-label="Stop responding" style="pointer-events:none"
                onclick="window.inertCount=(window.inertCount||0)+1">Stop</button>
            <section style="pointer-events:none"><button id="inert-child"
                aria-label="Stop generating" style="pointer-events:auto"
                onclick="window.inertChildCount=(window.inertChildCount||0)+1">Stop</button></section>
            <button id="real" aria-label="Stop generating"
                onclick="window.realCount=(window.realCount||0)+1">Stop</button>
        </body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let adapter = qwen::QwenAdapter {
            cfg: serde_yaml::from_str("name: qwen").unwrap(),
        };
        adapter.cancel_generation(&page);
        for field in [
            "ghostCount",
            "ghostChildCount",
            "inertCount",
            "inertChildCount",
        ] {
            let count = page.evaluate(&format!("window.{field} || 0"));
            assert_eq!(
                count.as_f64(),
                Some(0.0),
                "nonactionable {field} Stop must not be clicked"
            );
        }
        assert_eq!(page.evaluate("window.realCount || 0").as_f64(), Some(1.0));

        page.evaluate("document.querySelector('#real').hidden = true");
        adapter.cancel_generation(&page);
        assert_eq!(
            page.evaluate("window.realCount || 0").as_f64(),
            Some(1.0),
            "no candidate is actionable after hiding the only real Stop"
        );
    }
}

#[cfg(test)]
mod timeout_terminal_regression_tests {
    use super::*;

    #[tokio::test]
    async fn first_content_timeout_emits_only_a_typed_error_and_phase_metric() {
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
        // This is an explicitly labelled offline fixture, not a foreign
        // provider route: real browser pages never receive this test marker.
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture', '')",
        );

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
        assert!(failure
            .to_string()
            .contains("did not return new assistant content"));
        assert!(rx.recv().await.is_none(), "timeout must not emit done");
        assert_eq!(
            page.evaluate("window.cancelCount || 0").as_f64(),
            Some(1.0),
            "generation timeout should attempt to stop the in-flight provider response"
        );
        let output = metrics.render_with_provider_states(&[]);
        assert!(output.contains(
            "gateway_provider_timeout_total{provider=\"qwen\",phase=\"first_content\"} 1"
        ));
        assert!(!output
            .contains("gateway_provider_timeout_total{provider=\"qwen\",phase=\"generation\"} 1"));
        assert!(!output
            .contains("gateway_provider_timeout_total{provider=\"qwen\",phase=\"submission\"} 1"));
    }
    #[tokio::test]
    async fn partial_content_timeout_keeps_generation_phase_and_never_emits_done() {
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        let html = r#"<html><body><form>
            <textarea placeholder="Ask anything"></textarea>
            <button type="button" aria-label="Send" onclick="window.beginReply()">Send</button>
            <button type="button" id="stop" aria-label="Stop generation" hidden
                    onclick="window.cancelCount=(window.cancelCount||0)+1">Stop</button>
            </form><script>
              window.beginReply = () => {
                document.querySelector('#stop').hidden = false;
                const answer = document.createElement('div');
                answer.className = 'markdown-body';
                answer.textContent = 'partial fixture answer';
                document.body.appendChild(answer);
              };
            </script></body></html>"#;
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        // This is an explicitly labelled offline fixture, not a foreign
        // provider route: real browser pages never receive this test marker.
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture', '')",
        );
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
                "SYNTHETIC_PARTIAL".into(),
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

        let first = rx
            .recv()
            .await
            .expect("partial output is retained")
            .unwrap();
        assert!(!first.done);
        assert_eq!(first.content, "partial fixture answer");
        let failure = rx
            .recv()
            .await
            .expect("timeout follows partial output")
            .unwrap_err();
        assert!(failure
            .downcast_ref::<common::ProviderTimeoutError>()
            .is_some());
        assert!(failure.to_string().contains("did not complete"));
        assert!(
            rx.recv().await.is_none(),
            "partial reply cannot complete after timeout"
        );
        assert_eq!(page.evaluate("window.cancelCount || 0").as_f64(), Some(1.0));
        let output = metrics.render_with_provider_states(&[]);
        assert!(output
            .contains("gateway_provider_timeout_total{provider=\"qwen\",phase=\"generation\"} 1"));
        assert!(!output.contains(
            "gateway_provider_timeout_total{provider=\"qwen\",phase=\"first_content\"} 1"
        ));
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
        // This is an explicitly labelled offline fixture, not a foreign
        // provider route: real browser pages never receive this test marker.
        page.evaluate(
            "document.documentElement.setAttribute('data-chat-gateway-test-fixture', '')",
        );

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
            r#"<html data-chat-gateway-test-fixture><body><script>globalThis.fixtureLoaded=true</script><form><button id="unrelated-send" type="button" aria-label="Send message">Other</button></form><form>{markup}{button}</form></body></html>"#
        );
        let encoded: String = html.bytes().map(|byte| format!("%{byte:02X}")).collect();
        page.goto(&format!("data:text/html,{encoded}"))
            .await
            .unwrap();
        let js = r#"(() => {
          globalThis.unrelatedClicked = false;
          // A data: fixture cannot reach the real first-party account service.
          // Exercise the *actual* ChatGPT readiness fetch with a deliberately
          // synthetic, verified session; never skip or bypass that guard.
          globalThis.fixtureAuthChecks = 0;
          globalThis.fetch = async (url, options) => {
            if (url !== '/api/auth/session' || options?.credentials !== 'include' ||
                options?.cache !== 'no-store' || !options?.signal) {
              throw new Error('Unexpected fixture session request');
            }
            globalThis.fixtureAuthChecks++;
            return new Response(JSON.stringify({
              user:{fixture:true}, accessToken:'not-a-real-token',
              expires:new Date(Date.now() + 60000).toISOString()
            }));
          };
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
        let chatgpt_fixture = adapter.name() == "chatgpt";
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
        if chatgpt_fixture {
            assert_eq!(
                page.evaluate("globalThis.fixtureAuthChecks").as_f64(),
                Some(1.0),
                "the real first-party readiness guard must run before fixture Send"
            );
        }
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
