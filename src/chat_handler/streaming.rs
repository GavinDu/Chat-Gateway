//! SSE completion lifecycle with permit retention and transactional finalization.
use super::*;

pub(super) fn streamed_provider_response(attempt: StreamingAttempt) -> Response {
    let StreamingAttempt {
        state: state_c,
        owner_key_id: owner_key_id_c,
        windowed: windowed_c,
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
        first_chunk,
    } = attempt;
    let conversation_for_headers = conv_stream.clone();
    let provider_for_headers = provider_c.clone();
    let sse_stream = async_stream::stream! {
        let _permit = permit;
        let mut accumulator = chat_service::ResponseAccumulator::new(provider_started_c);
        let completion_deadline =
            chat_service::completion_deadline(provider_started_c, completion_timeout_c);
        // OpenAI-compatible clients expect the assistant role on the
        // first stream chunk, before content deltas.
        let role = sse_completion_chunk(
            &conv_stream,
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
            None => chat_service::recv_with_deadline(&mut rx, completion_deadline).await,
        } {
            let update = accumulator.push(item);
            let done_after_delta =
                matches!(&update, chat_service::StreamUpdate::CompleteWithDelta(_));
            match update {
                chat_service::StreamUpdate::Delta(content)
                | chat_service::StreamUpdate::CompleteWithDelta(content) => {
                    let data = sse_completion_chunk(
                        &conv_stream,
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
                    tracing::warn!(provider = %provider_c, "provider stream failed; partial response not committed");
                    break;
                }
            }
        }
        let collected = accumulator.finish();
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
                request_id: &request_id_c,
                provider: &provider_c,
                queue_ms: queue_ms_c,
                ttft_ms,
                provider_work_ms,
                request_started: request_started_c,
                stream_failed: stream_error.is_some(),
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
                tracing::error!(conversation_id = %conv_stream, %error, "conversation persistence failed after stream");
                let error = serde_json::json!({
                    "error": { "message": "conversation could not be persisted", "type": "server_error", "code": "memory_persistence_failed" }
                });
                yield Ok(Event::default().data(error.to_string()));
                yield Ok(Event::default().data("[DONE]"));
                return;
            }
            let completion_tokens = estimate_tokens(&full);
            let mut finish = sse_completion_chunk(
                &conv_stream,
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
