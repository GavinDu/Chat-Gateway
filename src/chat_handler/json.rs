//! Non-streaming JSON completion lifecycle, retry boundary, and durable commit.
use super::*;

pub(super) async fn json_provider_response(
    attempt: JsonAttempt<'_>,
) -> std::result::Result<Response, String> {
    let JsonAttempt {
        state,
        owner_key_id,
        windowed,
        provider,
        model,
        provider_model,
        conversation_id,
        request_id,
        request_started,
        provider_started,
        completion_timeout,
        queue_ms,
        prompt_tokens,
        receiver,
    } = attempt;
    let collected = state
        .chat_service
        .collect_with_timeout(receiver, provider_started, completion_timeout)
        .await;
    let full = collected.content;
    let failed = collected.error;
    let ttft_ms = collected
        .first_event_elapsed
        .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64);
    let provider_work_ms = provider_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let total_ms = request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    if let Some(error) = failed {
        state
            .provider_sessions
            .record_gateway_error(provider, &error);
        let message = error.to_string();
        chat_service::record_provider_outcome(
            &state.metrics,
            chat_service::ProviderOutcome {
                request_id,
                provider,
                outcome: "error",
                queue_ms,
                ttft_ms,
                provider_work_ms,
                total_ms,
            },
        );
        if full.is_empty() {
            tracing::warn!(request_id = %request_id, provider = %provider, error_kind = ?error.kind, retryable = error.retryable, "provider failed before output; considering fallback");
            return Err(message);
        }
        tracing::warn!(request_id = %request_id, provider = %provider, error_kind = ?error.kind, retryable = error.retryable, "provider failed after partial output");
        return Ok(api_error(
            StatusCode::BAD_GATEWAY,
            "provider_error",
            "partial_response",
            "provider failed after emitting a partial response; the turn was not committed",
        ));
    }

    let persist_error = chat_service::finalize_provider_attempt(
        state,
        owner_key_id,
        conversation_id,
        windowed,
        &full,
        model,
        chat_service::AttemptTelemetry {
            request_id,
            provider,
            queue_ms,
            ttft_ms,
            provider_work_ms,
            request_started,
            stream_failed: false,
        },
    )
    .await;
    if let Some(error) = persist_error {
        tracing::error!(conversation_id = %conversation_id, %error, "conversation persistence failed");
        return Ok(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "memory_persistence_failed",
            "conversation could not be persisted",
        ));
    }

    let body = json_completion_body(conversation_id, model, &full, prompt_tokens);
    let headers = completion_response_headers(conversation_id, provider, provider_model, false);
    Ok((headers, Json(body)).into_response())
}
