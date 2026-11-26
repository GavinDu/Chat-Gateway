//! Content-free Meta AI browser timings. Labels are a closed enum, never page data.
use crate::metrics::MetricsRegistry;
use std::{future::Future, time::Instant};

tokio::task_local! { static ACTIVE: MetricsRegistry; }

#[derive(Clone, Copy)]
pub(crate) enum Step {
    NavigationBeforeRequest,
    NavigationFetch,
    NavigationAfterResponse,
    NavigationUnobserved,
    RetentionSettle,
    RetentionSleep,
    RetentionProbe,
    InputSettleEngine,
    InputSettleSleep,
    AckSettle,
    AckSleep,
    ComposerSettle,
    ComposerSleep,
    ComposerProbe,
    CommandQueue,
    OperationGate,
    PageEnsure,
    Navigation,
    ComposerReady,
    ModelSelection,
    EntryGuards,
    BaselineProbe,
    Submission,
    ComposerWait,
    InputFill,
    InputSettle,
    DraftValidation,
    PasteDispatch,
    InputRetention,
    SendDispatch,
    SubmissionAck,
    AckProbe,
    BrowserSettle,
    BrowserSleep,
    FirstContentSettle,
    FirstContentSleep,
    FirstContentProbe,
    FirstContentProcessing,
    AfterContentSettle,
    AfterContentSleep,
    AfterContentProbe,
    AfterContentProcessing,
    FinalProbe,
    SessionSnapshot,
}
impl Step {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NavigationBeforeRequest => "navigation_before_request",
            Self::NavigationFetch => "navigation_fetch",
            Self::NavigationAfterResponse => "navigation_after_response",
            Self::NavigationUnobserved => "navigation_unobserved",
            Self::RetentionSettle => "retention_settle",
            Self::RetentionSleep => "retention_sleep",
            Self::RetentionProbe => "retention_probe",
            Self::InputSettleEngine => "input_settle_engine",
            Self::InputSettleSleep => "input_settle_sleep",
            Self::AckSettle => "ack_settle",
            Self::AckSleep => "ack_sleep",
            Self::ComposerSettle => "composer_settle",
            Self::ComposerSleep => "composer_sleep",
            Self::ComposerProbe => "composer_probe",
            Self::CommandQueue => "command_queue",
            Self::OperationGate => "operation_gate",
            Self::PageEnsure => "page_ensure",
            Self::Navigation => "navigation",
            Self::ComposerReady => "composer_ready",
            Self::ModelSelection => "model_selection",
            Self::EntryGuards => "entry_guards",
            Self::BaselineProbe => "baseline_probe",
            Self::Submission => "submission",
            Self::ComposerWait => "composer_wait",
            Self::InputFill => "input_fill",
            Self::InputSettle => "input_settle",
            Self::DraftValidation => "draft_validation",
            Self::PasteDispatch => "paste_dispatch",
            Self::InputRetention => "input_retention",
            Self::SendDispatch => "send_dispatch",
            Self::SubmissionAck => "submission_ack",
            Self::AckProbe => "ack_probe",
            Self::BrowserSettle => "browser_settle",
            Self::BrowserSleep => "browser_sleep",
            Self::FirstContentSettle => "first_content_settle",
            Self::FirstContentSleep => "first_content_sleep",
            Self::FirstContentProbe => "first_content_probe",
            Self::FirstContentProcessing => "first_content_processing",
            Self::AfterContentSettle => "after_content_settle",
            Self::AfterContentSleep => "after_content_sleep",
            Self::AfterContentProbe => "after_content_probe",
            Self::AfterContentProcessing => "after_content_processing",
            Self::FinalProbe => "final_probe",
            Self::SessionSnapshot => "session_snapshot",
        }
    }
}

pub(crate) async fn scope<F: Future>(metrics: MetricsRegistry, future: F) -> F::Output {
    ACTIVE.scope(metrics, future).await
}

/// Measures scope lifetime, including early error return or cancellation. Not a success counter.
/// Child intervals may nest; do not add them to their parent.
pub(crate) struct Timer {
    metaai: Option<(MetricsRegistry, Step, Instant)>,
    _chatgpt: Option<crate::providers::chatgpt::attempt_diagnostics::StepGuard>,
}
impl Timer {
    pub(crate) fn start(step: Step) -> Self {
        Self {
            metaai: ACTIVE
                .try_with(|metrics| (metrics.clone(), step, Instant::now()))
                .ok(),
            _chatgpt: crate::providers::chatgpt::attempt_diagnostics::start_step(step),
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((metrics, step, start)) = self.metaai.take() {
            metrics.record_metaai_step(
                step,
                start.elapsed().as_micros().min(u64::MAX as u128) as u64,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn scoped_timings_are_isolated_and_record_cancelled_intervals() {
        let metrics = MetricsRegistry::default();
        {
            let _timer = Timer::start(Step::Submission);
        }
        assert!(!metrics
            .render_with_provider_states(&[])
            .contains("stage=\"submission\""));
        scope(metrics.clone(), async {
            let _timer = Timer::start(Step::Submission);
            let result = tokio::time::timeout(std::time::Duration::from_millis(10), async {
                let _child = Timer::start(Step::SubmissionAck);
                std::future::pending::<()>().await;
            })
            .await;
            assert!(result.is_err());
        })
        .await;
        let text = metrics.render_with_provider_states(&[]);
        assert!(text.contains("gateway_metaai_step_duration_seconds_count{stage=\"submission\"} 1"));
        assert!(
            text.contains("gateway_metaai_step_duration_seconds_count{stage=\"submission_ack\"} 1")
        );
        let other = MetricsRegistry::default();
        assert!(!other
            .render_with_provider_states(&[])
            .contains("stage=\"submission\""));
    }
}

#[cfg(test)]
mod browser_tests {
    use super::*;
    #[tokio::test]
    async fn native_poll_drive_splits_engine_work_from_scheduled_sleep() {
        let metrics = MetricsRegistry::default();
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<main>timing fixture</main>")
            .await
            .unwrap();
        let start = Instant::now();
        scope(metrics.clone(), async {
            crate::providers::common::drive_page_timed(
                &mut page,
                std::time::Duration::from_millis(100),
                Step::FirstContentSettle,
                Step::FirstContentSleep,
            )
            .await;
        })
        .await;
        assert!(start.elapsed() >= std::time::Duration::from_millis(100));
        let text = metrics.render_with_provider_states(&[]);
        for stage in ["first_content_settle", "first_content_sleep"] {
            assert!(text.contains(&format!(
                "gateway_metaai_step_duration_seconds_count{{stage=\"{stage}\"}} 1"
            )));
        }
        assert!(!text.contains("timing fixture"));
    }
}

#[cfg(test)]
mod composer_tests {
    use super::*;
    #[tokio::test]
    async fn composer_wait_records_actual_drive_and_probe_children() {
        let metrics = MetricsRegistry::default();
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        page.goto("data:text/html,<main></main>").await.unwrap();
        page.evaluate("setTimeout(() => {const el=document.createElement('textarea');el.id='ready';document.body.appendChild(el);},20)");
        scope(metrics.clone(), async {
            crate::providers::common::wait_for_selector(&mut page, &["#ready"], 2000)
                .await
                .unwrap();
        })
        .await;
        let text = metrics.render_with_provider_states(&[]);
        for stage in ["composer_settle", "composer_sleep", "composer_probe"] {
            assert!(text.contains(&format!(
                "gateway_metaai_step_duration_seconds_count{{stage=\"{stage}\"}}"
            )));
        }
        assert!(!text.contains("stage=\"browser_settle\""));
    }
}

/// Passive, page-scoped navigation timestamps. Never read/store response content or headers.
/// The guard detaches callbacks on normal return, error and cancellation.
pub(crate) struct Navigation<'a> {
    page: &'a mut obscura_core::Page,
    observation: Option<NavigationObservation>,
}
#[derive(Default)]
struct NavigationState {
    request: Option<Instant>,
    response: Option<Instant>,
}
fn matches_document(info: &obscura_core::RequestInfo, target: &str) -> bool {
    info.resource_type == obscura_core::ResourceType::Document
        && info
            .url
            .join(target)
            .is_ok_and(|expected| info.url == expected)
}
struct NavigationObservation {
    metrics: MetricsRegistry,
    started: Instant,
    state: std::sync::Arc<std::sync::Mutex<NavigationState>>,
    ids: (u64, u64),
}
impl<'a> Navigation<'a> {
    pub(crate) fn new(page: &'a mut obscura_core::Page, target: &str) -> Self {
        let observation = ACTIVE.try_with(Clone::clone).ok().map(|metrics| {
            let started = Instant::now();
            let state = std::sync::Arc::new(std::sync::Mutex::new(NavigationState::default()));
            let request_state = state.clone();
            let expected = std::sync::Arc::<str>::from(target);
            let request_target = expected.clone();
            let request_id = page.on_request(std::sync::Arc::new(move |info| {
                if matches_document(info, request_target.as_ref()) {
                    if let Ok(mut state) = request_state.lock() {
                        if state.request.is_none() {
                            state.request = Some(Instant::now());
                        }
                    }
                }
            }));
            let response_state = state.clone();
            let response_id = page.on_response(std::sync::Arc::new(move |info, _response| {
                if matches_document(info, expected.as_ref()) {
                    if let Ok(mut state) = response_state.lock() {
                        if state.request.is_some() && state.response.is_none() {
                            state.response = Some(Instant::now());
                        }
                    }
                }
            }));
            NavigationObservation {
                metrics,
                started,
                state,
                ids: (request_id, response_id),
            }
        });
        Self { page, observation }
    }
    pub(crate) fn page(&mut self) -> &mut obscura_core::Page {
        self.page
    }
}
impl Drop for Navigation<'_> {
    fn drop(&mut self) {
        let Some(observation) = self.observation.take() else {
            return;
        };
        self.page.off_request(observation.ids.0);
        self.page.off_response(observation.ids.1);
        let ended = Instant::now();
        if let Ok(state) = observation.state.lock() {
            let record = |step, duration: std::time::Duration| {
                observation
                    .metrics
                    .record_metaai_step(step, duration.as_micros().min(u64::MAX as u128) as u64)
            };
            if let (Some(request), Some(response)) = (state.request, state.response) {
                record(
                    Step::NavigationBeforeRequest,
                    request.duration_since(observation.started),
                );
                record(Step::NavigationFetch, response.duration_since(request));
                record(
                    Step::NavigationAfterResponse,
                    ended.duration_since(response),
                );
            } else {
                record(
                    Step::NavigationUnobserved,
                    ended.duration_since(observation.started),
                );
            }
        };
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::*;
    #[tokio::test]
    async fn passive_navigation_observers_detach_even_without_a_network_response() {
        let metrics = MetricsRegistry::default();
        let browser = obscura_core::Browser::builder()
            .stealth(false)
            .build()
            .unwrap();
        let mut page = browser.new_page().await.unwrap();
        scope(metrics.clone(), async {
            let mut observation =
                Navigation::new(&mut page, "https://www.meta.ai/private-navigation-sentinel");
            let ids = observation.observation.as_ref().unwrap().ids;
            observation
                .page()
                .goto("data:text/html,<main>private navigation sentinel</main>")
                .await
                .unwrap();
            drop(observation);
            assert!(!page.off_request(ids.0));
            assert!(!page.off_response(ids.1));
        })
        .await;
        let text = metrics.render_with_provider_states(&[]);
        assert!(text.contains(
            "gateway_metaai_step_duration_seconds_count{stage=\"navigation_unobserved\"} 1"
        ));
        assert!(!text.contains("private navigation sentinel"));
    }
}

#[cfg(test)]
mod document_match_tests {
    use super::*;
    #[test]
    fn normalizes_root_url_and_rejects_other_documents_or_resource_types() {
        let mut info = obscura_core::RequestInfo {
            url: "https://www.meta.ai/".parse().unwrap(),
            method: "GET".into(),
            headers: Default::default(),
            resource_type: obscura_core::ResourceType::Document,
        };
        assert!(matches_document(&info, "https://www.meta.ai"));
        assert!(!matches_document(
            &info,
            "https://www.meta.ai/prompt/private"
        ));
        assert!(!matches_document(&info, "https://foreign.invalid/"));
        info.resource_type = obscura_core::ResourceType::Fetch;
        assert!(!matches_document(&info, "https://www.meta.ai/"));
    }
}
