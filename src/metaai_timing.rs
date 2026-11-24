//! Content-free Meta AI browser timings. Labels are a closed enum, never page data.
use crate::metrics::MetricsRegistry;
use std::{future::Future, time::Instant};

tokio::task_local! { static ACTIVE: MetricsRegistry; }

#[derive(Clone, Copy)]
pub(crate) enum Step {
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
pub(crate) struct Timer(Option<(MetricsRegistry, Step, Instant)>);
impl Timer {
    pub(crate) fn start(step: Step) -> Self {
        Self(
            ACTIVE
                .try_with(|metrics| (metrics.clone(), step, Instant::now()))
                .ok(),
        )
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((metrics, step, start)) = self.0.take() {
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
