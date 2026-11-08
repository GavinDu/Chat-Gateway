//! Request execution seam between the HTTP handler and provider backends.
//!
//! The service owns backend dispatch and common event-stream consumption so
//! streaming and non-streaming handlers share the same provider event contract.
use crate::chat_backend::{BackendRequest, ChatBackend, ChatEvent, GatewayError};
use crate::{
    api::api_error,
    app_state::{AppState, AuthKeyContext},
    budget_personal_memory_items,
    context::{self, ContextBuilder, ContextPlan, ThreadSyncState},
    conversation_context_read_error, is_reopenable_provider_thread_url,
    memory::{ChatMessage, LongTermMemoryStore, ProviderThread},
    memory_egress, persist_completed_turn, providers,
    read_conversation_context_with_transcript_for_owner, transcript_marker,
};
use anyhow::Result;
use axum::{extract::Extension, http::StatusCode, response::Response};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// Keep provider timing metrics and completion logs identical for streaming
/// and non-streaming requests. State transitions remain at the call site
/// because stream errors carry additional session-expiry classification.
pub(crate) struct ProviderOutcome<'a> {
    pub(crate) request_id: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) outcome: &'a str,
    pub(crate) queue_ms: u64,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) provider_work_ms: u64,
    pub(crate) total_ms: u64,
}

/// Classify the entire gateway operation, including durable turn commit.
/// Provider text alone is not successful API completion if persistence fails.
pub(crate) fn completed_chat_outcome(
    stream_failed: bool,
    persistence_failed: bool,
) -> &'static str {
    if stream_failed || persistence_failed {
        "error"
    } else {
        "success"
    }
}

pub(crate) fn record_provider_outcome(
    metrics_registry: &crate::metrics::MetricsRegistry,
    outcome: ProviderOutcome<'_>,
) {
    metrics_registry.record_provider_timings(
        outcome.provider,
        outcome.outcome,
        outcome.queue_ms,
        outcome.ttft_ms,
        outcome.provider_work_ms,
        outcome.total_ms,
    );
    tracing::info!(
        request_id = %outcome.request_id,
        provider = %outcome.provider,
        queue_ms = outcome.queue_ms,
        ttft_ms = ?outcome.ttft_ms,
        provider_work_ms = outcome.provider_work_ms,
        total_ms = outcome.total_ms,
        outcome = outcome.outcome,
        "provider request completed"
    );
}

pub struct ChatService {
    backend: Arc<dyn ChatBackend>,
}

/// A fully consumed provider response. Errors retain any partial content so
/// callers can enforce the rule that partial turns are never persisted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CollectedResponse {
    pub content: String,
    pub first_event_elapsed: Option<Duration>,
    pub error: Option<GatewayError>,
}

/// A channel closing without an explicit terminal event is never success.
/// Keep this constant shared by JSON and SSE consumers.
pub const INCOMPLETE_PROVIDER_STREAM: &str = "provider stream closed before terminal completion";
pub const EMPTY_PROVIDER_RESPONSE: &str = "provider completed without response content";

/// Convert a provider timeout into one absolute deadline, failing closed
/// on an unrepresentably large configured duration instead of panicking.
pub fn completion_deadline(started: Instant, max_wait: Duration) -> tokio::time::Instant {
    tokio::time::Instant::from_std(started)
        .checked_add(max_wait)
        .unwrap_or_else(tokio::time::Instant::now)
}

/// A fixed end-to-end completion deadline: an already-started stream must not
/// indefinitely hold a provider permit after its first delta. Both SSE and
/// JSON consumers use this helper, and the caller drops the receiver on error.
pub async fn recv_with_deadline(
    events: &mut mpsc::Receiver<Result<ChatEvent, GatewayError>>,
    deadline: tokio::time::Instant,
) -> Option<Result<ChatEvent, GatewayError>> {
    match tokio::time::timeout_at(deadline, events.recv()).await {
        Ok(item) => item,
        Err(_) => Some(Err(GatewayError::timeout("provider completion timed out"))),
    }
}

/// The event reducer is shared by SSE and JSON responses. Persistence is
/// permitted only after `Complete`; a partial response is never committed.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamUpdate {
    Delta(String),
    /// A terminal provider event may also contain the final response text.
    /// SSE must emit that final delta before the completion marker.
    CompleteWithDelta(String),
    Complete,
    Failed,
}

#[derive(Debug)]
pub struct ResponseAccumulator {
    attempt_started: Instant,
    collected: CollectedResponse,
    finished: bool,
    saw_done: bool,
}

impl ResponseAccumulator {
    pub fn new(attempt_started: Instant) -> Self {
        Self {
            attempt_started,
            collected: CollectedResponse::default(),
            finished: false,
            saw_done: false,
        }
    }

    /// Process a backend event exactly once, without depending on the
    /// transport's rendering or persistence policy.
    pub fn push(&mut self, event: std::result::Result<ChatEvent, GatewayError>) -> StreamUpdate {
        debug_assert!(!self.finished, "terminal backend event processed twice");
        match event {
            Ok(event) if event.done => {
                self.saw_done = true;
                self.finished = true;
                if event.content.is_empty() {
                    StreamUpdate::Complete
                } else {
                    if self.collected.first_event_elapsed.is_none() {
                        self.collected.first_event_elapsed = Some(self.attempt_started.elapsed());
                    }
                    self.collected.content.push_str(&event.content);
                    StreamUpdate::CompleteWithDelta(event.content)
                }
            }
            Ok(event) => {
                if !event.content.is_empty() && self.collected.first_event_elapsed.is_none() {
                    self.collected.first_event_elapsed = Some(self.attempt_started.elapsed());
                }
                self.collected.content.push_str(&event.content);
                StreamUpdate::Delta(event.content)
            }
            Err(error) => {
                self.finished = true;
                self.collected.error = Some(error);
                StreamUpdate::Failed
            }
        }
    }

    /// Finalize after an explicit done/error or a closed channel. Missing
    /// terminal events are errors even if content has already been emitted.
    pub fn finish(mut self) -> CollectedResponse {
        if self.collected.error.is_none() {
            if !self.saw_done {
                self.collected.error =
                    Some(GatewayError::provider_failure(INCOMPLETE_PROVIDER_STREAM));
            } else if self.collected.content.is_empty() {
                // Streaming already rejects an empty completion at first-event
                // admission. JSON must not persist that same empty response.
                self.collected.error =
                    Some(GatewayError::provider_failure(EMPTY_PROVIDER_RESPONSE));
            }
        }
        self.collected
    }
}

/// Record one failed provider admission at the backend-neutral service boundary.
/// Session state derives only from a typed error, never upstream text.
pub(crate) fn record_attempt_error(
    provider_sessions: &crate::provider_state::ProviderSessionRegistry,
    metrics_registry: &crate::metrics::MetricsRegistry,
    provider: &str,
    typed_error: Option<&GatewayError>,
    queue_ms: u64,
    provider_started: Instant,
    request_started: Instant,
) {
    let provider_work_ms = provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let total_ms = request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    // Provider text is untrusted; it must not decide whether the account needs
    // login. An absent typed error is a generic failed attempt, never an
    // inferred authentication failure from substrings in a page or exception.
    let generic_failure = GatewayError::provider_failure("provider attempt failed");
    provider_sessions.record_gateway_error(provider, typed_error.unwrap_or(&generic_failure));
    metrics_registry.record_provider_timings(
        provider,
        "error",
        queue_ms,
        None,
        provider_work_ms,
        total_ms,
    );
}

/// Backend admission failed before an event stream existed. A recoverable
/// worker/permit error is not evidence that a provider login has expired.
/// The metrics envelope and redacted state update belong to one service path.
pub(crate) fn record_backend_admission_error(
    provider_sessions: &crate::provider_state::ProviderSessionRegistry,
    metrics_registry: &crate::metrics::MetricsRegistry,
    provider: &str,
    error: &GatewayError,
    queue_ms: u64,
    provider_started: Instant,
    request_started: Instant,
) {
    tracing::debug!(
        provider,
        retryable = error.is_retryable(),
        "provider send failed before stream admission"
    );
    if !error.is_retryable() {
        provider_sessions.record_gateway_error(provider, error);
    }
    metrics_registry.record_provider_timings(
        provider,
        "error",
        queue_ms,
        None,
        provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    );
}

/// Fail closed before reserving a pacing slot: current-turn content and
/// transcript context must independently satisfy the provider egress policy.
pub(crate) fn authorize_candidate_context(
    policy: &memory_egress::MemoryEgressPolicy,
    provider: &str,
) -> std::result::Result<(), memory_egress::EgressError> {
    policy.authorize(provider, memory_egress::ProviderDataClass::CurrentTurn)?;
    policy.authorize(
        provider,
        memory_egress::ProviderDataClass::ConversationContext,
    )
}

/// Reserve request pacing before acquiring the single-page provider permit.
/// Holding the exclusive permit while waiting on rate limits would block
/// otherwise useful sends to the same provider.
pub(crate) async fn acquire_candidate_permit(
    state: &AppState,
    provider: &str,
    config: &crate::config::ProviderConfig,
) -> anyhow::Result<crate::browser::pool::ObscuraPermit> {
    state
        .provider_guard
        .wait_for_request_slot(
            provider,
            config.min_request_interval_ms,
            config.daily_request_cap,
        )
        .await?;
    state.pool.acquire(provider).await
}

/// The metrics timestamp is measured after the durable write for both JSON
/// and SSE. Provider work time is captured before that write, so the two
/// measurements remain distinct.
pub(crate) struct AttemptTelemetry<'a> {
    pub(crate) request_id: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) queue_ms: u64,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) provider_work_ms: u64,
    pub(crate) request_started: Instant,
    pub(crate) stream_failed: bool,
}

/// One completion boundary for both transports: never record a successful
/// gateway operation until the assistant turn is durable. A failed stream
/// neither writes the partial turn nor marks the provider session healthy.
pub(crate) async fn finalize_provider_attempt(
    state: &AppState,
    owner_key_id: &str,
    conversation_id: &str,
    user_messages: &[ChatMessage],
    assistant_content: &str,
    model: &str,
    telemetry: AttemptTelemetry<'_>,
) -> Option<anyhow::Error> {
    let persist_error = if telemetry.stream_failed {
        None
    } else {
        // Backend/session health is independent of the gateway's durable
        // write. A storage failure must be counted as a gateway error without
        // incorrectly degrading a provider that returned valid completion.
        state.provider_sessions.record_success(telemetry.provider);
        persist_completed_turn(
            state,
            owner_key_id,
            conversation_id,
            user_messages.to_vec(),
            assistant_content.to_owned(),
            telemetry.provider,
            model,
        )
        .await
        .err()
    };
    let total_ms = telemetry
        .request_started
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    record_provider_outcome(
        &state.metrics,
        ProviderOutcome {
            request_id: telemetry.request_id,
            provider: telemetry.provider,
            outcome: completed_chat_outcome(telemetry.stream_failed, persist_error.is_some()),
            queue_ms: telemetry.queue_ms,
            ttft_ms: telemetry.ttft_ms,
            provider_work_ms: telemetry.provider_work_ms,
            total_ms,
        },
    );
    persist_error
}

/// Resolve and authorize the provider candidates before acquiring a worker
/// permit. In particular, a failed affinity read or an empty API-key allowlist
/// must never fall through to a different provider.
pub(crate) async fn resolve_chat_candidates(
    state: &AppState,
    owner_key_id: &str,
    model: &str,
    conversation_id: Option<&str>,
    auth_context: Option<&Extension<AuthKeyContext>>,
) -> Result<(Vec<String>, String), Box<Response>> {
    let mut candidates = state
        .router
        .resolve_for_owner_async(owner_key_id, model, conversation_id, state.memory.clone())
        .await
        .map_err(|_| {
            tracing::error!(conversation_id = ?conversation_id, "provider affinity read failed");
            Box::new(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "memory_read_failed",
                "conversation routing state could not be read safely",
            ))
        })?;
    let original_candidate_count = candidates.len();
    if let Some(Extension(context)) = auth_context {
        if let Some(allowed) = &context.provider_allowlist {
            candidates.retain(|provider| allowed.iter().any(|item| item == provider));
            if candidates.is_empty() && original_candidate_count > 0 {
                return Err(Box::new(api_error(
                    StatusCode::FORBIDDEN,
                    "authentication_error",
                    "provider_not_allowed",
                    "the API key is not allowed to use the requested provider",
                )));
            }
        }
    }
    let provider_model = model
        .split_once('/')
        .map(|(_, model)| model)
        .unwrap_or(model)
        .to_owned();
    tracing::info!(model, ?candidates, conv = ?conversation_id, "routing request");
    if candidates.is_empty() {
        return Err(Box::new(api_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("model '{model}' is not configured for an enabled provider; use /v1/models or model='auto'"),
        )));
    }
    Ok((candidates, provider_model))
}

pub(crate) struct PreparedChatContext {
    pub(crate) memory_egress_policy: memory_egress::MemoryEgressPolicy,
    pub(crate) personal_memory_items: Vec<memory_egress::PersonalMemoryItem>,
    pub(crate) context_token_budget: usize,
    pub(crate) prompt: String,
    pub(crate) windowed: Vec<ChatMessage>,
    pub(crate) has_history: bool,
    pub(crate) stored_messages: Vec<ChatMessage>,
}

/// Prepare one immutable, owner-scoped context snapshot before provider
/// routing or admission. Both optional personal-memory reads and canonical
/// transcript reads fail closed; no provider is touched on a failed read.
pub(crate) async fn prepare_chat_context(
    state: &AppState,
    owner_key_id: &str,
    conversation_id: Option<&str>,
    incoming_messages: &[ChatMessage],
) -> Result<PreparedChatContext, Box<Response>> {
    let memory_egress_policy = memory_egress::MemoryEgressPolicy::from_env().map_err(|_| {
        Box::new(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_egress_policy_invalid",
            "provider memory egress policy is invalid",
        ))
    })?;

    // Do not reserve memory context tokens when no provider is authorized
    // to receive personal items. Disabled personal memory requires no read.
    let retrieved_memory = if !memory_egress_policy
        .allowed_personal_memory_providers()
        .is_empty()
    {
        state
            .memory
            .clone()
            .retrieve_long_term_memory_items_async(owner_key_id.to_owned(), 20)
            .await
            .map_err(|_| {
                Box::new(api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "memory_read_failed",
                    "long-term memory could not be read safely",
                ))
            })?
    } else {
        Vec::new()
    };
    let (personal_memory_items, memory_tokens) =
        budget_personal_memory_items(retrieved_memory, 512);
    let context_token_budget = 8000usize.saturating_sub(memory_tokens);

    let (prompt, windowed, has_history, stored_messages) =
        read_conversation_context_with_transcript_for_owner(
            state.memory.clone(),
            owner_key_id.to_owned(),
            conversation_id.map(str::to_owned),
            incoming_messages.to_vec(),
            context_token_budget,
        )
        .await
        .map_err(|_| Box::new(conversation_context_read_error(conversation_id)))?;
    if prompt.is_empty() {
        return Err(Box::new(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "empty_prompt",
            "prompt must not be empty",
        )));
    }

    Ok(PreparedChatContext {
        memory_egress_policy,
        personal_memory_items,
        context_token_budget,
        prompt,
        windowed,
        has_history,
        stored_messages,
    })
}

/// The pure provider-context decision is shared by JSON and SSE attempts.
///
/// A delta is allowed only for a validated, reopenable thread whose exact
/// provider, model and durable transcript marker still match. Otherwise
/// return an isolated bounded replay and never navigate the old thread.
pub(crate) fn plan_provider_send(
    provider: &str,
    model: &str,
    stored_messages: &[ChatMessage],
    incoming_messages: &[ChatMessage],
    saved_thread: Option<ProviderThread>,
    token_budget: usize,
    replay_prompt: &str,
) -> (String, Option<String>) {
    let saved_thread = saved_thread
        .filter(|thread| is_reopenable_provider_thread_url(provider, &thread.thread_url));
    let supports_reopen = providers::ADAPTERS
        .iter()
        .find(|spec| spec.id == provider)
        .is_some_and(|spec| matches!(spec.threads, providers::ThreadSupport::UrlAddressable))
        && saved_thread
            .as_ref()
            .is_some_and(|thread| thread.model.as_deref() == Some(model));
    let last_marker = transcript_marker(stored_messages);
    let thread_state = saved_thread.as_ref().map(|thread| ThreadSyncState {
        provider: thread.provider.clone(),
        thread_url: thread.thread_url.clone(),
        synced_through_message_id: thread.synced_through_message_id.clone(),
        last_stored_message_id: Some(last_marker),
    });
    match ContextBuilder::plan(
        stored_messages,
        incoming_messages,
        provider,
        thread_state.as_ref(),
        supports_reopen,
        token_budget,
    ) {
        ContextPlan::Delta { messages } => (
            context::render_prompt(&messages),
            saved_thread.map(|thread| thread.thread_url),
        ),
        ContextPlan::Replay { preamble, .. } => (format!("{preamble}\n{replay_prompt}"), None),
    }
}

impl ChatService {
    pub fn new(backend: Arc<dyn ChatBackend>) -> Self {
        Self { backend }
    }

    pub async fn stream(
        &self,
        request: BackendRequest,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        let backend_id = self.backend.id();
        let capabilities = self.backend.capabilities();
        tracing::debug!(
            backend = backend_id.as_str(),
            streaming = capabilities.streaming,
            model_selection = capabilities.model_selection,
            system_role = capabilities.system_role,
            addressable_threads = capabilities.addressable_threads,
            "executing request through chat backend"
        );
        self.backend.send_stream(request).await
    }

    /// A backend may stall *before* it returns a stream receiver (for example
    /// while its command queue is saturated). Bound admission as well as event
    /// collection; dropping the pending future cancels the admission attempt.
    pub async fn stream_with_timeout(
        &self,
        request: BackendRequest,
        max_wait: Duration,
    ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError> {
        match tokio::time::timeout(max_wait, self.stream(request)).await {
            Ok(result) => result,
            Err(_) => Err(GatewayError::timeout("provider submission timed out")),
        }
    }

    /// Consume a provider stream for a non-SSE response using the same event
    /// semantics as the streaming path. The caller supplies the attempt start
    /// instant so TTFT includes backend startup and first-event wait.
    #[cfg(test)] // Unbounded collection is retained only for isolated reducer tests.
    pub async fn collect(
        &self,
        events: mpsc::Receiver<Result<ChatEvent, GatewayError>>,
        attempt_started: Instant,
    ) -> CollectedResponse {
        self.collect_inner(events, attempt_started, None).await
    }

    /// Use the configured provider completion deadline in production. A
    /// backend that keeps the channel open without a terminal event must not
    /// hang an HTTP request forever. Dropping the receiver also cancels the
    /// producer through the existing backend cancellation contract.
    pub async fn collect_with_timeout(
        &self,
        events: mpsc::Receiver<Result<ChatEvent, GatewayError>>,
        attempt_started: Instant,
        max_wait: Duration,
    ) -> CollectedResponse {
        self.collect_inner(events, attempt_started, Some(max_wait))
            .await
    }

    async fn collect_inner(
        &self,
        mut events: mpsc::Receiver<Result<ChatEvent, GatewayError>>,
        attempt_started: Instant,
        max_wait: Option<Duration>,
    ) -> CollectedResponse {
        let mut accumulator = ResponseAccumulator::new(attempt_started);
        let deadline = max_wait.map(|budget| completion_deadline(attempt_started, budget));
        loop {
            let next = if let Some(deadline) = deadline {
                recv_with_deadline(&mut events, deadline).await
            } else {
                events.recv().await
            };
            let Some(item) = next else {
                break;
            };
            if !matches!(accumulator.push(item), StreamUpdate::Delta(_)) {
                break;
            }
        }
        accumulator.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::ChatService;
    use crate::chat_backend::{
        test_support::{ScriptedBackend, ScriptedSend},
        BackendRequest, ChatBackend, ChatEvent, GatewayError, GatewayErrorKind, ProviderId,
    };
    use anyhow::Result;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::mpsc;

    #[test]
    fn context_egress_is_checked_for_both_classes_before_admission() {
        let policy = crate::memory_egress::MemoryEgressPolicy::from_allowlist("")
            .expect("empty personal-memory allowlist is valid");
        assert!(super::authorize_candidate_context(&policy, "chatgpt").is_ok());
        assert!(super::authorize_candidate_context(&policy, "claude").is_ok());
        assert!(super::authorize_candidate_context(&policy, "unknown-provider").is_err());
    }

    #[test]
    fn admission_errors_preserve_typed_auth_state_and_retryable_worker_liveness() {
        let sessions = crate::provider_state::ProviderSessionRegistry::default();
        let metrics = crate::metrics::MetricsRegistry::default();
        let provider = "chatgpt";
        sessions.record_success(provider);
        let now = Instant::now();
        super::record_backend_admission_error(
            &sessions,
            &metrics,
            provider,
            &GatewayError::backend_unavailable("synthetic worker failure"),
            0,
            now,
            now,
        );
        assert_eq!(
            sessions.status(provider).state,
            crate::provider_state::ProviderSessionState::Ready
        );
        super::record_backend_admission_error(
            &sessions,
            &metrics,
            provider,
            &GatewayError::session_expired("synthetic expired session"),
            0,
            now,
            now,
        );
        assert_eq!(
            sessions.status(provider).state,
            crate::provider_state::ProviderSessionState::NeedsLogin
        );
    }

    fn scripted_request() -> BackendRequest {
        BackendRequest {
            provider: ProviderId::new("chatgpt").unwrap(),
            prompt: "fixture prompt".to_owned(),
            model: "test-model".to_owned(),
            has_history: false,
            thread_url: None,
        }
    }

    #[tokio::test]
    async fn delayed_backend_admission_times_out_and_next_request_can_succeed() {
        let service = ChatService::new(Arc::new(ScriptedBackend::new(vec![
            ScriptedSend::DelayAdmission(std::time::Duration::from_secs(2), vec![]),
            ScriptedSend::Events(vec![Ok(ChatEvent {
                content: "recovered".into(),
                done: true,
            })]),
        ])));
        let error = match service
            .stream_with_timeout(scripted_request(), std::time::Duration::from_millis(20))
            .await
        {
            Ok(_) => panic!("delayed admission must not hold the caller indefinitely"),
            Err(error) => error,
        };
        assert_eq!(error.kind, GatewayErrorKind::Timeout);
        assert!(error.is_retryable());
        let rx = service
            .stream_with_timeout(scripted_request(), std::time::Duration::from_secs(1))
            .await
            .expect("subsequent request must still be admitted");
        let result = service
            .collect_with_timeout(
                rx,
                std::time::Instant::now(),
                std::time::Duration::from_secs(1),
            )
            .await;
        assert_eq!(result.content, "recovered");
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn scriptable_backend_distinguishes_admission_rejection_and_midstream_failure() {
        let service = ChatService::new(Arc::new(ScriptedBackend::new(vec![
            ScriptedSend::Reject(GatewayError::provider_busy("provider occupied")),
            ScriptedSend::Events(vec![
                Ok(ChatEvent {
                    content: "partial answer".into(),
                    done: false,
                }),
                Err(GatewayError::provider_failure("provider disconnected")),
            ]),
        ])));

        let rejected = match service.stream(scripted_request()).await {
            Ok(_) => panic!("busy admission must fail before a stream exists"),
            Err(error) => error,
        };
        assert_eq!(rejected.kind, GatewayErrorKind::ProviderBusy);
        assert!(rejected.is_retryable());

        let response = service
            .stream(scripted_request())
            .await
            .expect("second scripted send starts");
        let collected = service.collect(response, std::time::Instant::now()).await;
        assert_eq!(collected.content, "partial answer");
        assert_eq!(
            collected.error.unwrap().kind,
            GatewayErrorKind::ProviderFailure
        );
    }

    #[tokio::test]
    async fn scriptable_backend_models_delayed_content_and_idle_cancellation() {
        use std::time::Duration;
        use tokio::sync::oneshot;
        let (cancelled_tx, cancelled_rx) = oneshot::channel();
        let service = ChatService::new(Arc::new(ScriptedBackend::new(vec![
            ScriptedSend::Delay(
                Duration::from_millis(40),
                vec![
                    Ok(ChatEvent {
                        content: "delayed".into(),
                        done: false,
                    }),
                    Ok(ChatEvent {
                        content: String::new(),
                        done: true,
                    }),
                ],
            ),
            ScriptedSend::Hold(cancelled_tx),
        ])));

        let mut delayed = service.stream(scripted_request()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), delayed.recv())
                .await
                .is_err(),
            "delayed first content should not be mistaken for immediate completion"
        );
        let first = tokio::time::timeout(Duration::from_secs(1), delayed.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.content, "delayed");
        assert!(!first.done);
        assert!(delayed.recv().await.unwrap().unwrap().done);
        drop(delayed);

        let idle = service.stream(scripted_request()).await.unwrap();
        drop(idle);
        tokio::time::timeout(Duration::from_secs(1), cancelled_rx)
            .await
            .expect("dropping an idle provider stream must cancel the sender")
            .expect("scripted sender should observe cancellation");
    }

    struct FakeBackend;

    #[async_trait::async_trait]
    impl ChatBackend for FakeBackend {
        async fn models(
            &self,
            _provider: crate::chat_backend::ProviderId,
        ) -> std::result::Result<Vec<String>, GatewayError> {
            Ok(vec!["test-model".to_owned()])
        }

        async fn send_stream(
            &self,
            request: BackendRequest,
        ) -> std::result::Result<mpsc::Receiver<Result<ChatEvent, GatewayError>>, GatewayError>
        {
            let (tx, rx) = mpsc::channel(2);
            tx.send(Ok(ChatEvent {
                content: format!("{}:{}", request.provider.as_str(), request.prompt),
                done: false,
            }))
            .await
            .expect("test receiver remains open");
            tx.send(Ok(ChatEvent {
                content: String::new(),
                done: true,
            }))
            .await
            .expect("test receiver remains open");
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn streaming_deadline_returns_typed_error_after_first_content() {
        use super::{recv_with_deadline, ResponseAccumulator, StreamUpdate};
        let (sender, mut receiver) = mpsc::channel(2);
        let started = std::time::Instant::now();
        sender
            .send(Ok(ChatEvent {
                content: "partial stream".to_owned(),
                done: false,
            }))
            .await
            .unwrap();
        let deadline =
            tokio::time::Instant::from_std(started) + std::time::Duration::from_millis(15);
        let mut accumulator = ResponseAccumulator::new(started);
        let first = recv_with_deadline(&mut receiver, deadline).await.unwrap();
        assert_eq!(
            accumulator.push(first),
            StreamUpdate::Delta("partial stream".to_owned())
        );
        let timeout = recv_with_deadline(&mut receiver, deadline).await.unwrap();
        assert_eq!(accumulator.push(timeout), StreamUpdate::Failed);
        let collected = accumulator.finish();
        assert_eq!(collected.content, "partial stream");
        assert_eq!(
            collected.error.as_ref().unwrap().kind,
            crate::chat_backend::GatewayErrorKind::Timeout
        );
        drop(receiver);
        assert!(sender.is_closed());
    }

    #[tokio::test]
    async fn collect_deadline_rejects_silent_open_backend_and_cancels_receiver() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (sender, rx) = mpsc::channel(2);
        let result = service
            .collect_with_timeout(
                rx,
                std::time::Instant::now(),
                std::time::Duration::from_millis(15),
            )
            .await;
        assert!(result.content.is_empty());
        assert_eq!(
            result.error.as_ref().unwrap().kind,
            crate::chat_backend::GatewayErrorKind::Timeout
        );
        assert!(
            sender.is_closed(),
            "timed-out receiver should cancel producer"
        );
    }

    #[tokio::test]
    async fn collect_deadline_preserves_partial_content_as_failed_not_committable() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (sender, rx) = mpsc::channel(2);
        sender
            .send(Ok(ChatEvent {
                content: "partial answer".to_owned(),
                done: false,
            }))
            .await
            .unwrap();
        let result = service
            .collect_with_timeout(
                rx,
                std::time::Instant::now(),
                std::time::Duration::from_millis(15),
            )
            .await;
        assert_eq!(result.content, "partial answer");
        assert_eq!(
            result.error.as_ref().unwrap().kind,
            crate::chat_backend::GatewayErrorKind::Timeout
        );
        assert!(sender.is_closed(), "partial timeout should cancel producer");
    }

    #[tokio::test]
    async fn collect_deadline_allows_prompt_terminal_completion() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (sender, rx) = mpsc::channel(2);
        sender
            .send(Ok(ChatEvent {
                content: "complete".to_owned(),
                done: false,
            }))
            .await
            .unwrap();
        sender
            .send(Ok(ChatEvent {
                content: String::new(),
                done: true,
            }))
            .await
            .unwrap();
        let result = service
            .collect_with_timeout(
                rx,
                std::time::Instant::now(),
                std::time::Duration::from_secs(2),
            )
            .await;
        assert_eq!(result.content, "complete");
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn service_forwards_prepared_request_to_injected_backend() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let mut events = service
            .stream(BackendRequest {
                provider: ProviderId::new("claude").unwrap(),
                prompt: "test prompt".to_owned(),
                model: "test-model".to_owned(),
                has_history: true,
                thread_url: Some("https://provider.invalid/thread/1".to_owned()),
            })
            .await
            .expect("fake backend should accept the request");
        let event = events.recv().await.unwrap().unwrap();
        assert_eq!(event.content, "claude:test prompt");
        assert!(!event.done);
        assert!(events.recv().await.unwrap().unwrap().done);
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn collect_aggregates_until_done_and_records_first_event_latency() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let events = service
            .stream(BackendRequest {
                provider: ProviderId::new("claude").unwrap(),
                prompt: "hello".to_owned(),
                model: "test-model".to_owned(),
                has_history: false,
                thread_url: None,
            })
            .await
            .unwrap();
        let collected = service.collect(events, std::time::Instant::now()).await;
        assert_eq!(collected.content, "claude:hello");
        assert!(collected.error.is_none());
        assert!(collected.first_event_elapsed.is_some());
    }

    #[tokio::test]
    async fn collect_does_not_count_empty_chunks_as_first_content() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();

        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert!(collected.content.is_empty());
        assert_eq!(
            collected.error.unwrap().message,
            super::EMPTY_PROVIDER_RESPONSE,
            "an empty terminal event must not commit a successful JSON turn"
        );
        assert!(
            collected.first_event_elapsed.is_none(),
            "empty heartbeat events must not be reported as TTFT"
        );
    }

    #[tokio::test]
    async fn collect_starts_ttft_at_first_non_empty_content_after_heartbeats() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(3);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: "answer".to_owned(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();

        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "answer");
        assert!(
            collected.first_event_elapsed.is_some(),
            "the first non-empty content event must establish TTFT"
        );
    }

    #[tokio::test]
    async fn collect_preserves_partial_content_but_reports_terminal_error() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(ChatEvent {
            content: "partial".into(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Err(GatewayError::provider_failure("upstream failed")))
            .await
            .unwrap();
        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "partial");
        assert_eq!(collected.error.unwrap().message, "upstream failed");
    }

    #[tokio::test]
    async fn collect_rejects_closed_stream_without_terminal_completion() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(ChatEvent {
            content: "partial answer".into(),
            done: false,
        }))
        .await
        .unwrap();
        drop(tx);

        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "partial answer");
        let error = collected.error.expect("missing terminal event must fail");
        assert!(error.message.contains("closed before terminal completion"));
    }

    #[tokio::test]
    async fn same_reducer_drives_json_collection_and_sse_success() {
        use super::{ResponseAccumulator, StreamUpdate};
        let start = std::time::Instant::now();
        let scripted = vec![
            Ok(ChatEvent {
                content: String::new(),
                done: false,
            }),
            Ok(ChatEvent {
                content: "first".to_owned(),
                done: false,
            }),
            Ok(ChatEvent {
                content: " second".to_owned(),
                done: false,
            }),
            Ok(ChatEvent {
                content: String::new(),
                done: true,
            }),
        ];
        let (tx, rx) = mpsc::channel(8);
        for item in scripted.iter().cloned() {
            tx.send(item).await.unwrap();
        }
        drop(tx);
        let json = ChatService::new(Arc::new(FakeBackend))
            .collect(rx, start)
            .await;
        let mut sse = ResponseAccumulator::new(start);
        let emitted: Vec<StreamUpdate> = scripted.into_iter().map(|item| sse.push(item)).collect();
        assert_eq!(
            emitted,
            vec![
                StreamUpdate::Delta(String::new()),
                StreamUpdate::Delta("first".to_owned()),
                StreamUpdate::Delta(" second".to_owned()),
                StreamUpdate::Complete,
            ]
        );
        let streamed = sse.finish();
        assert_eq!(streamed.content, json.content);
        assert!(streamed.error.is_none());
        assert!(json.error.is_none());
        assert!(streamed.first_event_elapsed.is_some());
        assert!(json.first_event_elapsed.is_some());
    }

    #[tokio::test]
    async fn terminal_content_is_not_lost_in_json_or_sse() {
        use super::{ResponseAccumulator, StreamUpdate};

        for (events, expected) in [
            (
                vec![ChatEvent {
                    content: "only terminal content".to_owned(),
                    done: true,
                }],
                "only terminal content",
            ),
            (
                vec![
                    ChatEvent {
                        content: "first".to_owned(),
                        done: false,
                    },
                    ChatEvent {
                        content: " final".to_owned(),
                        done: true,
                    },
                ],
                "first final",
            ),
        ] {
            let started = std::time::Instant::now();
            let (tx, rx) = mpsc::channel(4);
            for event in &events {
                tx.send(Ok(event.clone())).await.unwrap();
            }
            drop(tx);
            let json = ChatService::new(Arc::new(FakeBackend))
                .collect(rx, started)
                .await;

            let mut sse = ResponseAccumulator::new(started);
            let updates: Vec<_> = events
                .into_iter()
                .map(|event| sse.push(Ok(event)))
                .collect();
            assert!(matches!(
                updates.last(),
                Some(StreamUpdate::CompleteWithDelta(_))
            ));
            let streamed = sse.finish();
            assert_eq!(json.content, expected);
            assert_eq!(streamed.content, expected);
            assert!(json.error.is_none());
            assert!(streamed.error.is_none());
            assert!(json.first_event_elapsed.is_some());
            assert!(streamed.first_event_elapsed.is_some());
        }
    }

    #[tokio::test]
    async fn same_reducer_fails_closed_for_midstream_error_and_channel_close() {
        use super::{ResponseAccumulator, StreamUpdate, INCOMPLETE_PROVIDER_STREAM};
        for missing_done in [true, false] {
            let start = std::time::Instant::now();
            let mut scripted = vec![Ok(ChatEvent {
                content: "partial".to_owned(),
                done: false,
            })];
            if !missing_done {
                scripted.push(Err(GatewayError::provider_failure("backend failure")));
            }
            let (tx, rx) = mpsc::channel(8);
            for item in scripted.iter().cloned() {
                tx.send(item).await.unwrap();
            }
            drop(tx);
            let json = ChatService::new(Arc::new(FakeBackend))
                .collect(rx, start)
                .await;
            let mut sse = ResponseAccumulator::new(start);
            let last = scripted.into_iter().map(|item| sse.push(item)).last();
            assert_eq!(
                last,
                Some(if missing_done {
                    StreamUpdate::Delta("partial".to_owned())
                } else {
                    StreamUpdate::Failed
                })
            );
            let streamed = sse.finish();
            assert_eq!(streamed.content, "partial");
            assert_eq!(streamed.content, json.content);
            assert_eq!(streamed.error, json.error);
            assert_eq!(
                streamed.error.unwrap().message,
                if missing_done {
                    INCOMPLETE_PROVIDER_STREAM
                } else {
                    "backend failure"
                }
            );
        }
    }

    #[tokio::test]
    async fn collect_treats_done_as_terminal_and_ignores_later_events() {
        let service = ChatService::new(Arc::new(FakeBackend));
        let (tx, rx) = mpsc::channel(3);
        tx.send(Ok(ChatEvent {
            content: "answer".into(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: "must-not-append".into(),
            done: false,
        }))
        .await
        .unwrap();
        let collected = service.collect(rx, std::time::Instant::now()).await;
        assert_eq!(collected.content, "answer");
        assert!(collected.error.is_none());
    }
}

/// First meaningful event observed while starting a provider stream.
///
/// Empty non-terminal chunks are ignored; a terminal event without content is
/// distinct from a closed channel and from an upstream error so the router can
/// preserve its existing pre-output failover behavior.
#[derive(Debug)]
pub enum FirstEvent {
    Chunk(ChatEvent),
    EmptyCompletion,
    Error(GatewayError),
    Closed,
    Timeout,
}

/// Keep first-event failure semantics within the transport-independent
/// service. The HTTP handler records each classified failure once and
/// preserves the typed auth/challenge error when an upstream event carried it.
/// Empty terminal events, closed streams and timeouts remain distinct failover
/// reasons, without interpreting untrusted provider response text.
pub(crate) fn classify_initial_stream_event(
    first: FirstEvent,
) -> std::result::Result<ChatEvent, (String, Option<GatewayError>)> {
    match first {
        FirstEvent::Chunk(chunk) => Ok(chunk),
        FirstEvent::EmptyCompletion => Err((
            "provider completed without output before stream started".to_owned(),
            None,
        )),
        FirstEvent::Error(error) => Err((error.to_string(), Some(error))),
        FirstEvent::Closed => Err((
            "provider stream closed before first output".to_owned(),
            None,
        )),
        FirstEvent::Timeout => Err(("provider timed out before first output".to_owned(), None)),
    }
}

pub(crate) fn usable_initial_stream_chunk(content: &str, _done: bool) -> bool {
    // A provider may send its complete response in one terminal event. Treat
    // nonempty final content as the first meaningful chunk, not an empty reply.
    !content.is_empty()
}

/// Wait for the first meaningful event without allowing an empty stream to
/// stall provider failover indefinitely.
pub async fn first_event(
    rx: &mut mpsc::Receiver<Result<ChatEvent, GatewayError>>,
    timeout: Duration,
) -> FirstEvent {
    match tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Some(Ok(chunk)) if usable_initial_stream_chunk(&chunk.content, chunk.done) => {
                    break Some(Ok(chunk));
                }
                Some(Ok(chunk)) if chunk.done => break Some(Ok(chunk)),
                Some(Ok(_empty_chunk)) => continue,
                Some(Err(error)) => break Some(Err(error)),
                None => break None,
            }
        }
    })
    .await
    {
        Ok(Some(Ok(chunk))) if usable_initial_stream_chunk(&chunk.content, chunk.done) => {
            FirstEvent::Chunk(chunk)
        }
        Ok(Some(Ok(_))) => FirstEvent::EmptyCompletion,
        Ok(Some(Err(error))) => FirstEvent::Error(error),
        Ok(None) => FirstEvent::Closed,
        Err(_) => FirstEvent::Timeout,
    }
}

#[cfg(test)]
mod first_event_tests {
    use super::{first_event, FirstEvent};
    use crate::chat_backend::{ChatEvent, GatewayError};
    use anyhow::Result;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn first_event_skips_empty_chunks_and_returns_first_content() {
        let (tx, mut rx) = mpsc::channel(3);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: false,
        }))
        .await
        .unwrap();
        tx.send(Ok(ChatEvent {
            content: "answer".into(),
            done: false,
        }))
        .await
        .unwrap();
        let event = first_event(&mut rx, Duration::from_secs(1)).await;
        assert!(matches!(event, FirstEvent::Chunk(chunk) if chunk.content == "answer"));
    }

    #[tokio::test]
    async fn terminal_event_with_content_is_a_valid_first_stream_chunk() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Ok(ChatEvent {
            content: "complete answer".into(),
            done: true,
        }))
        .await
        .unwrap();

        let event = first_event(&mut rx, Duration::from_secs(1)).await;
        assert!(matches!(
            event,
            FirstEvent::Chunk(chunk) if chunk.done && chunk.content == "complete answer"
        ));
    }

    #[tokio::test]
    async fn first_event_distinguishes_empty_completion_from_closed_stream() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Ok(ChatEvent {
            content: String::new(),
            done: true,
        }))
        .await
        .unwrap();
        assert!(matches!(
            first_event(&mut rx, Duration::from_secs(1)).await,
            FirstEvent::EmptyCompletion
        ));
        drop(tx);
        assert!(matches!(
            first_event(&mut rx, Duration::from_secs(1)).await,
            FirstEvent::Closed
        ));
    }

    #[tokio::test]
    async fn first_event_preserves_backend_error() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Err(GatewayError::provider_failure("upstream failed")))
            .await
            .unwrap();
        assert!(
            matches!(first_event(&mut rx, Duration::from_secs(1)).await, FirstEvent::Error(error) if error.message == "upstream failed")
        );
    }

    #[tokio::test]
    async fn first_event_times_out_when_provider_never_emits() {
        let (_tx, mut rx) = mpsc::channel::<Result<ChatEvent, GatewayError>>(1);
        assert!(matches!(
            first_event(&mut rx, Duration::from_millis(1)).await,
            FirstEvent::Timeout
        ));
    }
}
