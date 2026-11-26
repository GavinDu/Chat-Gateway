//! Content-free observations for one ChatGPT browser attempt. This records
//! partial scope lifetimes on cancellation; it never certifies a successful send.
use crate::{metaai_timing::Step, providers::common::ResponseProbe};
use serde::Serialize;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

tokio::task_local! { static ACTIVE: Rc<Context>; }
const MAX_US: u64 = 1_800_000_000;
const MAX_COUNT: u64 = 1_000_000;
fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_micros().min(MAX_US as u128) as u64
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Queue,
    PageRestore,
    Navigation,
    AccountReadiness,
    ComposerReady,
    ModelSelection,
    EntryGuards,
    Baseline,
    Submission,
    FirstContent,
    AfterContent,
    SessionSnapshot,
}
impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::Queue => "operation_admission",
            Self::PageRestore => "page_restore",
            Self::Navigation => "navigation",
            Self::AccountReadiness => "account_readiness",
            Self::ComposerReady => "composer_ready",
            Self::ModelSelection => "model_selection",
            Self::EntryGuards => "entry_guards",
            Self::Baseline => "baseline",
            Self::Submission => "submission",
            Self::FirstContent => "first_content",
            Self::AfterContent => "after_content",
            Self::SessionSnapshot => "session_snapshot",
        }
    }
}
#[derive(Clone, Default, Serialize)]
struct Timing {
    count: u64,
    sum_us: u64,
    max_us: u64,
}
impl Timing {
    fn add(&mut self, us: u64) {
        self.count = self.count.saturating_add(1).min(MAX_COUNT);
        self.sum_us = self.sum_us.saturating_add(us).min(MAX_US);
        self.max_us = self.max_us.max(us.min(MAX_US));
    }
}
struct Observation {
    sequence: Option<u64>,
    command_queue_us: u64,
    consumer_cancelled: bool,
    content_evidence: &'static str,
    started: Instant,
    stage: Stage,
    stage_started: Instant,
    phases: BTreeMap<&'static str, Timing>,
    steps: BTreeMap<&'static str, Timing>,
    pending: BTreeMap<u64, (&'static str, Instant)>,
    next_id: u64,
    baseline: Option<Sample>,
    recent: VecDeque<Sample>,
    polls: u64,
    poll_streaming_seen: bool,
    poll_affordance_seen: bool,
    poll_nonbaseline_text_seen: bool,
    max_poll_response_count: usize,
    max_poll_populated_count: usize,
    first_content_seen: bool,
    terminal_attempted: bool,
    submission_returned: bool,
}
impl Observation {
    fn new(sequence: Option<u64>) -> Self {
        let now = Instant::now();
        Self {
            sequence,
            command_queue_us: 0,
            consumer_cancelled: false,
            content_evidence: "none",
            started: now,
            stage: Stage::Queue,
            stage_started: now,
            phases: BTreeMap::new(),
            steps: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_id: 0,
            baseline: None,
            recent: VecDeque::new(),
            polls: 0,
            poll_streaming_seen: false,
            poll_affordance_seen: false,
            poll_nonbaseline_text_seen: false,
            max_poll_response_count: 0,
            max_poll_populated_count: 0,
            first_content_seen: false,
            terminal_attempted: false,
            submission_returned: false,
        }
    }
    fn transition(&mut self, stage: Stage) {
        if self.stage == stage {
            return;
        }
        self.phases
            .entry(self.stage.label())
            .or_default()
            .add(elapsed(self.stage_started));
        self.stage = stage;
        self.stage_started = Instant::now();
    }
    fn report(&self, returned: bool) -> serde_json::Value {
        let mut phases = self.phases.clone();
        phases
            .entry(self.stage.label())
            .or_default()
            .add(elapsed(self.stage_started));
        let mut steps = self.steps.clone();
        for (label, start) in self.pending.values() {
            steps.entry(label).or_default().add(elapsed(*start));
        }
        serde_json::json!({"schema_version":1,"provider":"chatgpt","sequence":self.sequence,"sequence_available":self.sequence.is_some(),"command_queue_us":self.command_queue_us,"consumer_cancelled":self.consumer_cancelled,"scope":"browser_attempt","future_returned":returned,
            "last_stage":self.stage.label(),"elapsed_us":elapsed(self.started),"phases":phases,"steps":steps,
            "baseline":self.baseline,"recent_samples":self.recent,"polls":self.polls,
            "poll_streaming_seen":self.poll_streaming_seen,"poll_affordance_seen":self.poll_affordance_seen,
            "poll_nonbaseline_text_seen":self.poll_nonbaseline_text_seen,"max_poll_response_count":self.max_poll_response_count,"max_poll_populated_count":self.max_poll_populated_count,
            "submission_returned":self.submission_returned,"first_content_seen":self.first_content_seen,
            "terminal_attempted":self.terminal_attempted,"content_evidence":self.content_evidence,"timing_semantics":"scope_lifetime_including_cancellation"})
    }
}
struct Context {
    state: RefCell<Observation>,
    sequence: Option<u64>,
    dropped_steps: Cell<u64>,
    dropped_updates: Cell<u64>,
}
impl Context {
    fn new(sequence: Option<u64>) -> Self {
        let state = Observation::new(sequence);
        let sequence = state.sequence;
        Self {
            state: RefCell::new(state),
            sequence,
            dropped_steps: Cell::new(0),
            dropped_updates: Cell::new(0),
        }
    }
    fn bump(counter: &Cell<u64>) {
        counter.set(counter.get().saturating_add(1).min(MAX_COUNT));
    }
}
fn update(f: impl FnOnce(&mut Observation)) {
    let _ = ACTIVE.try_with(|state| {
        if let Ok(mut data) = state.state.try_borrow_mut() {
            f(&mut data);
        } else {
            Context::bump(&state.dropped_updates);
        }
    });
}
pub(crate) fn command_queue(duration: Duration) {
    update(|s| s.command_queue_us = duration.as_micros().min(MAX_US as u128) as u64);
}
pub(crate) fn consumer_cancelled() {
    update(|s| s.consumer_cancelled = true);
}
pub(crate) fn stage(stage: Stage) {
    update(|s| s.transition(stage));
}
pub(crate) fn baseline(probe: &ResponseProbe) {
    update(|s| s.baseline = Some(sample(probe, None)));
}
pub(crate) fn poll(probe: &ResponseProbe, baseline: Option<&str>) {
    update(|s| {
        s.polls = s.polls.saturating_add(1).min(MAX_COUNT);
        if s.recent.len() == 8 {
            s.recent.pop_front();
        }
        let point = sample(probe, baseline);
        s.poll_streaming_seen |= point.streaming;
        s.poll_affordance_seen |= point.post_answer_affordance;
        s.poll_nonbaseline_text_seen |= point.text_present && !point.matches_baseline;
        s.max_poll_response_count = s.max_poll_response_count.max(point.count);
        s.max_poll_populated_count = s.max_poll_populated_count.max(point.populated_count);
        s.recent.push_back(point);
    });
}
pub(crate) fn first_content() {
    update(|s| {
        s.first_content_seen = true;
        s.content_evidence = "fresh_delta";
    });
}
pub(crate) fn verified_identical_reply() {
    update(|s| {
        s.first_content_seen = true;
        s.content_evidence = "verified_identical_reply";
    });
}
pub(crate) fn terminal_attempted() {
    update(|s| s.terminal_attempted = true);
}
pub(crate) fn submission_returned() {
    update(|s| s.submission_returned = true);
}

pub(crate) struct StepGuard {
    state: Rc<Context>,
    id: u64,
}
pub(crate) fn start_step(step: Step) -> Option<StepGuard> {
    ACTIVE
        .try_with(|state| {
            let mut s = match state.state.try_borrow_mut() {
                Ok(s) => s,
                Err(_) => {
                    Context::bump(&state.dropped_updates);
                    return None;
                }
            };
            if s.pending.len() >= 64 {
                Context::bump(&state.dropped_steps);
                return None;
            }
            s.next_id = match s.next_id.checked_add(1) {
                Some(id) => id,
                None => {
                    Context::bump(&state.dropped_steps);
                    return None;
                }
            };
            let id = s.next_id;
            s.pending.insert(id, (step.label(), Instant::now()));
            Some(StepGuard {
                state: state.clone(),
                id,
            })
        })
        .ok()
        .flatten()
}
impl Drop for StepGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.state.try_borrow_mut() {
            if let Some((label, start)) = s.pending.remove(&self.id) {
                s.steps.entry(label).or_default().add(elapsed(start));
            }
        } else {
            Context::bump(&self.state.dropped_updates);
        }
    }
}
struct Reporter {
    state: Rc<Context>,
    returned: bool,
    sink: Rc<dyn Fn(serde_json::Value)>,
}
impl Drop for Reporter {
    fn drop(&mut self) {
        let incomplete = self.state.sequence.is_none()
            || self.state.dropped_steps.get() > 0
            || self.state.dropped_updates.get() > 0;
        let mut report = match self.state.state.try_borrow() {
            Ok(s) => s.report(self.returned),
            Err(_) => {
                serde_json::json!({"schema_version":1,"provider":"chatgpt","scope":"browser_attempt","sequence":self.state.sequence,"sequence_available":self.state.sequence.is_some(),
                "future_returned":self.returned,"observation_incomplete":true,"reporting_error":"state_borrowed"})
            }
        };
        if report.get("observation_incomplete").is_none() {
            report["observation_incomplete"] = incomplete.into();
        }
        report["dropped_steps"] = self.state.dropped_steps.get().into();
        report["dropped_updates"] = self.state.dropped_updates.get().into();
        (self.sink)(report);
    }
}
async fn with_sequence<F: Future>(
    sequence: Option<u64>,
    future: F,
    sink: Rc<dyn Fn(serde_json::Value)>,
) -> F::Output {
    let state = Rc::new(Context::new(sequence));
    let mut reporter = Reporter {
        state: state.clone(),
        returned: false,
        sink,
    };
    let output = ACTIVE.scope(state, future).await;
    reporter.returned = true;
    output
}
#[cfg(test)]
async fn with_sink<F: Future>(future: F, sink: Rc<dyn Fn(serde_json::Value)>) -> F::Output {
    with_sequence(Some(1), future, sink).await
}
pub(crate) async fn scope<F: Future>(
    metrics: crate::metrics::MetricsRegistry,
    future: F,
) -> F::Output {
    with_sequence(metrics.next_chatgpt_attempt_sequence(),future,Rc::new(|report| {
        tracing::info!(observation=%report,"ChatGPT browser attempt observation (content-free)");
    })).await
}

#[derive(Clone, Default, Serialize, Debug)]
pub(super) struct Sample {
    count: usize,
    populated_count: usize,
    text_bytes: usize,
    text_present: bool,
    matches_baseline: bool,
    streaming: bool,
    post_answer_affordance: bool,
    challenge: bool,
}
pub(super) fn sample(probe: &ResponseProbe, baseline: Option<&str>) -> Sample {
    Sample {
        count: probe.count.min(4096),
        populated_count: probe.populated_count.min(4096),
        text_bytes: probe
            .text
            .as_ref()
            .map(|t| t.len().min(1048576))
            .unwrap_or(0),
        text_present: probe.text.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
        matches_baseline: matches!(baseline,Some(b) if probe.text.as_deref()==Some(b)),
        streaming: probe.streaming,
        post_answer_affordance: probe.post_answer_affordance,
        challenge: probe.challenge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metaai_timing::Timer;
    use std::time::Duration;
    #[test]
    fn sample_privacy_and_bounds() {
        let p = ResponseProbe {
            text: Some("secret".to_string()),
            url: "https://secret.example/x".to_string(),
            count: 9999,
            populated_count: 5000,
            ..ResponseProbe::default()
        };
        let s = sample(&p, None);
        let j = serde_json::to_string(&s).unwrap();
        assert!(!j.contains("secret"));
        assert!(!j.contains("secret.example"));
        assert_eq!(s.count, 4096);
        assert_eq!(s.populated_count, 4096);
        assert_eq!(s.text_bytes, 6);
        assert!(s.text_present);
        assert!(!s.matches_baseline);
        let n = sample(&ResponseProbe::default(), None);
        assert!(!n.matches_baseline);
        assert!(!n.text_present);
        let e = sample(
            &ResponseProbe {
                text: Some("x".to_string()),
                ..ResponseProbe::default()
            },
            Some("x"),
        );
        assert!(e.matches_baseline);
    }

    #[tokio::test]
    async fn outer_deadline_retains_active_stage_steps_and_redacted_probe() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = reports.clone();
        let result = tokio::time::timeout(
            Duration::from_millis(15),
            with_sink(
                async {
                    stage(Stage::Submission);
                    {
                        let _step = Timer::start(Step::InputFill);
                        tokio::task::yield_now().await;
                    }
                    submission_returned();
                    stage(Stage::FirstContent);
                    let p = ResponseProbe {
                        text: Some("private text".to_owned()),
                        url: "https://chatgpt.com/c/private".to_owned(),
                        count: 1,
                        populated_count: 1,
                        ..Default::default()
                    };
                    baseline(&p);
                    poll(&p, Some("private text"));
                    let _step = Timer::start(Step::FirstContentSettle);
                    std::future::pending::<()>().await;
                },
                Rc::new(move |report| sink.borrow_mut().push(report)),
            ),
        )
        .await;
        assert!(result.is_err());
        let reports = reports.borrow();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r["future_returned"], false);
        assert_eq!(r["last_stage"], "first_content");
        assert_eq!(r["submission_returned"], true);
        assert_eq!(r["first_content_seen"], false);
        assert_eq!(r["steps"]["input_fill"]["count"], 1);
        assert_eq!(r["steps"]["first_content_settle"]["count"], 1);
        assert!(
            r["steps"]["first_content_settle"]["sum_us"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(r["recent_samples"][0]["matches_baseline"], true);
        assert!(!r.to_string().contains("private"));
        assert!(!r.to_string().contains("https:"));
    }
    #[tokio::test]
    async fn parallel_attempts_and_metaai_metrics_remain_isolated() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let a = reports.clone();
        let b = reports.clone();
        let metrics = crate::metrics::MetricsRegistry::default();
        tokio::join!(
            with_sink(
                async {
                    stage(Stage::Submission);
                    let _s = Timer::start(Step::Submission);
                    tokio::task::yield_now().await;
                    first_content();
                    terminal_attempted();
                },
                Rc::new(move |r| a.borrow_mut().push(r))
            ),
            with_sink(
                async {
                    stage(Stage::Navigation);
                    let _s = Timer::start(Step::Navigation);
                    tokio::task::yield_now().await;
                },
                Rc::new(move |r| b.borrow_mut().push(r))
            ),
            crate::metaai_timing::scope(metrics.clone(), async {
                let _s = Timer::start(Step::ModelSelection);
                tokio::task::yield_now().await;
            })
        );
        let reports = reports.borrow();
        assert_eq!(reports.len(), 2);
        let submitted = reports
            .iter()
            .find(|r| r["last_stage"] == "submission")
            .unwrap();
        assert_eq!(submitted["future_returned"], true);
        assert_eq!(submitted["first_content_seen"], true);
        assert_eq!(submitted["terminal_attempted"], true);
        assert!(submitted["steps"].get("navigation").is_none());
        assert!(submitted["steps"].get("model_selection").is_none());
        let navigating = reports
            .iter()
            .find(|r| r["last_stage"] == "navigation")
            .unwrap();
        assert_eq!(navigating["first_content_seen"], false);
        let rendered = metrics.render_with_provider_states(&[]);
        assert!(rendered.contains("stage=\"model_selection\""));
        assert!(!rendered.contains("stage=\"submission\""));
        assert!(!rendered.contains("stage=\"navigation\""));
    }
    #[test]
    fn sample_and_all_timing_values_are_bounded() {
        let mut s = Observation::new(Some(1));
        s.steps.entry("submission").or_default().add(u64::MAX);
        // Snapshot caps durations even for hostile test-supplied measurements.
        assert_eq!(s.steps["submission"].sum_us, MAX_US);
        let huge = ResponseProbe {
            text: Some("x".repeat(1_048_577)),
            ..Default::default()
        };
        assert_eq!(sample(&huge, None).text_bytes, 1_048_576);
    }
    #[tokio::test]
    async fn poll_ring_retains_only_eight_bounded_samples() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = reports.clone();
        with_sink(
            async {
                command_queue(Duration::from_secs(2000));
                for count in 0..100 {
                    poll(
                        &ResponseProbe {
                            count,
                            populated_count: usize::MAX,
                            ..Default::default()
                        },
                        None,
                    );
                }
                consumer_cancelled();
            },
            Rc::new(move |r| sink.borrow_mut().push(r)),
        )
        .await;
        let reports = reports.borrow();
        let r = &reports[0];
        assert_eq!(r["polls"], 100);
        assert_eq!(r["recent_samples"].as_array().unwrap().len(), 8);
        assert_eq!(r["recent_samples"][0]["count"], 92);
        assert_eq!(r["recent_samples"][7]["count"], 99);
        assert_eq!(r["recent_samples"][7]["populated_count"], 4096);
        assert_eq!(r["consumer_cancelled"], true);
        assert_eq!(r["command_queue_us"], MAX_US);
    }
    #[test]
    fn bounded_and_borrow_conflicts_are_reported_without_panics() {
        let state = Rc::new(Context::new(Some(1)));
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = reports.clone();
        let reporter = Reporter {
            state: state.clone(),
            returned: false,
            sink: Rc::new(move |r| sink.borrow_mut().push(r)),
        };
        let held = state.state.borrow_mut();
        drop(reporter);
        drop(held);
        let reports = reports.borrow();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0]["observation_incomplete"], true);
        assert_eq!(reports[0]["reporting_error"], "state_borrowed");
    }
    #[tokio::test]
    async fn observer_limits_and_reentrant_updates_are_explicit() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = reports.clone();
        with_sink(
            async {
                let mut guards = Vec::new();
                for _ in 0..65 {
                    guards.push(start_step(Step::Submission));
                }
                ACTIVE.with(|state| {
                    let _held = state.state.borrow_mut();
                    first_content();
                });
                drop(guards);
            },
            Rc::new(move |r| sink.borrow_mut().push(r)),
        )
        .await;
        let reports = reports.borrow();
        let r = &reports[0];
        assert_eq!(r["dropped_steps"], 1);
        assert_eq!(r["dropped_updates"], 1);
        assert_eq!(r["observation_incomplete"], true);
        assert_eq!(r["first_content_seen"], false);
    }
    #[tokio::test]
    async fn guarded_identical_reply_has_explicit_evidence_category() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = reports.clone();
        with_sink(
            async {
                verified_identical_reply();
            },
            Rc::new(move |r| sink.borrow_mut().push(r)),
        )
        .await;
        assert_eq!(reports.borrow()[0]["first_content_seen"], true);
        assert_eq!(
            reports.borrow()[0]["content_evidence"],
            "verified_identical_reply"
        );
    }
    #[tokio::test]
    async fn cancelled_inner_send_must_not_look_like_successful_worker() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink_reports = reports.clone();
        let sink: Rc<dyn Fn(serde_json::Value)> =
            Rc::new(move |v| sink_reports.borrow_mut().push(v));
        with_sink(
            async {
                stage(Stage::FirstContent);
                let result = tokio::time::timeout(Duration::from_millis(5), async {
                    let _timer = Timer::start(Step::FirstContentSettle);
                    std::future::pending::<()>().await;
                })
                .await;
                assert!(result.is_err());
                consumer_cancelled();
                tokio::task::yield_now().await;
            },
            sink,
        )
        .await;
        let reports = reports.borrow();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r["future_returned"], true);
        assert_eq!(r["consumer_cancelled"], true);
        assert_eq!(r["terminal_attempted"], false);
        assert_eq!(r["first_content_seen"], false);
        assert_eq!(r["last_stage"], "first_content");
        assert_eq!(r["steps"]["first_content_settle"]["count"], 1);
    }
    #[tokio::test]
    async fn unavailable_runtime_sequence_is_explicitly_incomplete() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = reports.clone();
        with_sequence(None, async {}, Rc::new(move |r| sink.borrow_mut().push(r))).await;
        let reports = reports.borrow();
        let r = &reports[0];
        assert!(r["sequence"].is_null());
        assert_eq!(r["sequence_available"], false);
        assert_eq!(r["observation_incomplete"], true);
    }
    #[tokio::test]
    async fn poll_aggregates_survive_ring_eviction_without_retaining_text() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = reports.clone();
        with_sink(
            async {
                poll(
                    &ResponseProbe {
                        text: Some("synthetic-private".to_owned()),
                        count: 4000,
                        populated_count: 3000,
                        streaming: true,
                        post_answer_affordance: true,
                        ..Default::default()
                    },
                    None,
                );
                for _ in 0..10 {
                    poll(&ResponseProbe::default(), None);
                }
            },
            Rc::new(move |r| sink.borrow_mut().push(r)),
        )
        .await;
        let reports = reports.borrow();
        let r = &reports[0];
        assert_eq!(r["recent_samples"][0]["text_present"], false);
        assert_eq!(r["max_poll_response_count"], 4000);
        assert_eq!(r["max_poll_populated_count"], 3000);
        assert_eq!(r["poll_streaming_seen"], true);
        assert_eq!(r["poll_affordance_seen"], true);
        assert_eq!(r["poll_nonbaseline_text_seen"], true);
        assert!(!r.to_string().contains("synthetic-private"));
    }
}
