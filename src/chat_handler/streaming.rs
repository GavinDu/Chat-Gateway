//! SSE completion lifecycle with permit retention and transactional finalization.
use super::*;

/// SSE-local observation only: this is not a browser cleanup acknowledgement
/// or a durable transcript commit. Never retain provider text in the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservedTerminal {
    Pending,
    Done,
    Error(chat_backend::GatewayErrorKind),
    ChannelClosed,
}

struct SseAttemptDiagnostics {
    request_id: String,
    provider: String,
    started: Instant,
    content_events: usize,
    content_bytes: usize,
    first_observed_ms: Option<u64>,
    last_observed_ms: Option<u64>,
    terminal: ObservedTerminal,
    reducer_finished: bool,
}

impl SseAttemptDiagnostics {
    fn new(request_id: String, provider: String, started: Instant) -> Self {
        Self {
            request_id,
            provider,
            started,
            content_events: 0,
            content_bytes: 0,
            first_observed_ms: None,
            last_observed_ms: None,
            terminal: ObservedTerminal::Pending,
            reducer_finished: false,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn observe_chunk(&mut self, chunk: &chat_backend::ChatEvent) {
        if !chunk.content.is_empty() {
            let elapsed = self.elapsed_ms();
            self.first_observed_ms.get_or_insert(elapsed);
            self.last_observed_ms = Some(elapsed);
            self.content_events = self.content_events.saturating_add(1);
            self.content_bytes = self.content_bytes.saturating_add(chunk.content.len());
        }
        if chunk.done {
            self.terminal = ObservedTerminal::Done;
        }
    }

    fn observe(&mut self, event: &Result<chat_backend::ChatEvent, chat_backend::GatewayError>) {
        match event {
            Ok(chunk) => self.observe_chunk(chunk),
            Err(error) => self.terminal = ObservedTerminal::Error(error.kind),
        }
    }

    fn reducer_finished(&mut self) {
        if self.terminal == ObservedTerminal::Pending {
            self.terminal = ObservedTerminal::ChannelClosed;
        }
        self.reducer_finished = true;
        self.record("reducer_finished");
    }

    fn record(&self, boundary: &'static str) {
        tracing::info!(
            request_id = %self.request_id,
            provider = %self.provider,
            boundary,
            observed_terminal = ?self.terminal,
            content_events = self.content_events,
            content_bytes = self.content_bytes,
            first_observed_ms = ?self.first_observed_ms,
            last_observed_ms = ?self.last_observed_ms,
            observed_elapsed_ms = self.elapsed_ms(),
            "SSE provider attempt observation"
        );
    }
}

impl Drop for SseAttemptDiagnostics {
    fn drop(&mut self) {
        if !self.reducer_finished {
            // This also covers a response dropped before its first poll.
            // Do not infer why it was dropped or that the worker has stopped.
            self.record("body_dropped_before_reducer_finish");
        }
    }
}

pub(super) fn streamed_provider_response(attempt: StreamingAttempt) -> Response {
    let StreamingAttempt {
        state: state_c,
        owner_key_id: owner_key_id_c,
        windowed: windowed_c,
        snapshot_marker: snapshot_marker_c,
        snapshot_identity: snapshot_identity_c,
        provider: provider_c,
        model: model_c,
        provider_model: provider_model_c,
        conversation_id: conv_stream,
        request_id: request_id_c,
        request_started: request_started_c,
        provider_started: provider_started_c,
        completion_timeout: completion_timeout_c,
        queue_ms: queue_ms_c,
        prompt_tokens: prompt_tokens_c,
        permit,
        receiver: mut rx,
        thread_observation,
        first_chunk,
    } = attempt;
    let conversation_for_headers = conv_stream.clone();
    let provider_for_headers = provider_c.clone();
    let identity = CompletionIdentity::new();
    // Capture the already-admitted first chunk even if the body is never polled.
    let mut diagnostics = SseAttemptDiagnostics::new(
        request_id_c.clone(),
        provider_c.clone(),
        provider_started_c,
    );
    diagnostics.observe_chunk(&first_chunk);
    let sse_stream = async_stream::stream! {
        let mut diagnostics = diagnostics;
        let _permit = permit;
        let mut accumulator = chat_service::ResponseAccumulator::new(provider_started_c);
        let completion_deadline =
            chat_service::completion_deadline(provider_started_c, completion_timeout_c);
        // OpenAI-compatible clients expect the assistant role on the
        // first stream chunk, before content deltas.
        let role = sse_completion_chunk(
            &identity,
            &model_c,
            serde_json::json!({ "role": "assistant" }),
            None,
        );
        yield Ok::<_, anyhow::Error>(Event::default().data(role.to_string()));
        // The same terminal-aware reducer drives both SSE and JSON
        // responses, including TTFT, partial errors, and channel closure.
        let mut pending: Option<Result<chat_backend::ChatEvent, chat_backend::GatewayError>> = Some(Ok(first_chunk));
        while let Some(item) = match pending.take() {
            Some(item) => Some(item),
            None => {
                let item = chat_service::recv_with_deadline(&mut rx, completion_deadline).await;
                if let Some(event) = item.as_ref() {
                    diagnostics.observe(event);
                }
                item
            },
        } {
            let update = accumulator.push(item);
            let done_after_delta =
                matches!(&update, chat_service::StreamUpdate::CompleteWithDelta(_));
            match update {
                chat_service::StreamUpdate::Delta(content)
                | chat_service::StreamUpdate::CompleteWithDelta(content) => {
                    let data = sse_completion_chunk(
                        &identity,
                        &model_c,
                        serde_json::json!({ "content": content }),
                        None,
                    );
                    yield Ok::<_, anyhow::Error>(Event::default().data(data.to_string()));
                    if done_after_delta {
                        break;
                    }
                }
                chat_service::StreamUpdate::Complete => break,
                chat_service::StreamUpdate::Failed => {
                    tracing::warn!(request_id = %request_id_c, provider = %provider_c, "provider stream failed; partial response not committed");
                    break;
                }
            }
        }
        let collected = accumulator.finish();
        diagnostics.reducer_finished();
        let full = collected.content;
        let ttft_ms = collected
            .first_event_elapsed
            .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64);
        let provider_gateway_error = collected.error;
        let stream_error = provider_gateway_error.as_ref().map(|error| {
            if error.message == chat_service::INCOMPLETE_PROVIDER_STREAM {
                "provider stream closed before terminal completion; partial response was not committed".to_owned()
            } else {
                "provider stream failed; partial response was not committed".to_owned()
            }
        });

        let provider_work_ms = provider_started_c.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let persist_error = chat_service::finalize_provider_attempt(
            &state_c,
            &owner_key_id_c,
            &conv_stream,
            &windowed_c,
            &full,
            &model_c,
            chat_service::AttemptTelemetry {
                snapshot_marker: &snapshot_marker_c,
                snapshot_identity: &snapshot_identity_c,
                request_id: &request_id_c,
                provider: &provider_c,
                queue_ms: queue_ms_c,
                ttft_ms,
                provider_work_ms,
                request_started: request_started_c,
                stream_failed: stream_error.is_some(),
                thread_observation: thread_observation.and_then(|mut rx| rx.try_recv().ok()),
            },
        )
        .await;
        if let Some(message) = stream_error {
            // Only typed provider authentication failures require
            // re-login. Never infer that from arbitrary upstream text.
            if let Some(error) = provider_gateway_error.as_ref() {
                state_c
                    .provider_sessions
                    .record_gateway_error(&provider_c, error);
            }
            let error = serde_json::json!({
                "error": { "message": message, "type": "provider_error", "code": "stream_failed" }
            });
            yield Ok(Event::default().data(error.to_string()));
        } else {
            if let Some(error) = persist_error {
                tracing::error!(
                    request_id = %request_id_c,
                    provider = %provider_c,
                    failure_category = persistence_failure_log_category(&error),
                    "conversation persistence failed after stream"
                );
                let error = serde_json::json!({
                    "error": { "message": "conversation could not be persisted", "type": "server_error", "code": "memory_persistence_failed" }
                });
                yield Ok(Event::default().data(error.to_string()));
                yield Ok(Event::default().data("[DONE]"));
                return;
            }
            let completion_tokens = estimate_tokens(&full);
            let mut finish = sse_completion_chunk(
                &identity,
                &model_c,
                serde_json::json!({}),
                Some("stop"),
            );
            finish["usage"] = serde_json::json!({
                "prompt_tokens": prompt_tokens_c,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens_c + completion_tokens,
            });
            finish["x_usage_estimated"] = serde_json::json!(true);
            yield Ok(Event::default().data(finish.to_string()));
        }
        yield Ok(Event::default().data("[DONE]"));
    };
    let headers = completion_response_headers(
        &conversation_for_headers,
        &provider_for_headers,
        &provider_model_c,
        true,
    );
    (headers, Sse::new(sse_stream)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics() -> SseAttemptDiagnostics {
        SseAttemptDiagnostics::new("request-test".into(), "chatgpt".into(), Instant::now())
    }

    fn chunk(content: &str, done: bool) -> chat_backend::ChatEvent {
        chat_backend::ChatEvent {
            content: content.into(),
            done,
        }
    }

    #[test]
    fn partial_error_uses_only_typed_category_and_content_lengths() {
        let mut observed = diagnostics();
        observed.observe_chunk(&chunk("é", false));
        observed.observe(&Ok(chunk("", false)));
        observed.observe(&Ok(chunk("abc", false)));
        observed.observe(&Err(chat_backend::GatewayError::timeout("SECRET_SENTINEL")));
        observed.reducer_finished();
        assert_eq!(
            observed.terminal,
            ObservedTerminal::Error(chat_backend::GatewayErrorKind::Timeout)
        );
        assert_eq!(observed.content_events, 2);
        assert_eq!(observed.content_bytes, 5);
        assert!(observed.first_observed_ms.is_some());
        assert!(observed.last_observed_ms >= observed.first_observed_ms);
        assert!(observed.reducer_finished);
    }

    #[test]
    fn closed_channel_and_empty_terminal_are_not_conflated() {
        let mut closed = diagnostics();
        closed.reducer_finished();
        assert_eq!(closed.terminal, ObservedTerminal::ChannelClosed);
        assert_eq!(closed.first_observed_ms, None);

        let mut empty = diagnostics();
        empty.observe_chunk(&chunk("", true));
        empty.reducer_finished();
        assert_eq!(empty.terminal, ObservedTerminal::Done);
        assert_eq!(empty.content_events, 0);
        assert_eq!(empty.content_bytes, 0);
    }

    #[test]
    fn admitted_first_chunk_is_observed_before_body_poll_and_not_a_commit() {
        let mut observed = diagnostics();
        observed.observe_chunk(&chunk("first", true));
        assert_eq!(observed.terminal, ObservedTerminal::Done);
        assert_eq!(observed.content_events, 1);
        assert!(!observed.reducer_finished);
        let next = diagnostics();
        assert_eq!(next.terminal, ObservedTerminal::Pending);
        assert_eq!(next.content_bytes, 0);
        assert_eq!(next.first_observed_ms, None);
    }
}
