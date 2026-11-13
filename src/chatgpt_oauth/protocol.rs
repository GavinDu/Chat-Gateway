//! Bounded Responses SSE parsing. EOF, [DONE], and text are not completion.
use super::{failure, GatewayError};
use crate::chat_backend::{ChatEvent, GatewayErrorKind};
use serde_json::Value;

const MAX_EVENT: usize = 1_048_576;
#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: String,
    terminal: bool,
    saw_text: bool,
    text_bytes: usize,
}
impl Decoder {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<ChatEvent>, GatewayError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.terminal {
                break;
            }
            if byte != b'\n' {
                if self.line.len() >= MAX_EVENT {
                    return Err(failure("ChatGPT SSE event exceeds the size limit"));
                }
                self.line.push(byte);
                continue;
            }
            let raw = std::mem::take(&mut self.line);
            let line = std::str::from_utf8(&raw)
                .map_err(|_| failure("ChatGPT SSE contains invalid UTF-8"))?
                .trim_end_matches('\r');
            if line.is_empty() {
                if let Some(event) = self.flush()? {
                    events.push(event);
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                if self.data.len().saturating_add(data.len()).saturating_add(1) > MAX_EVENT {
                    return Err(failure("ChatGPT SSE event exceeds the size limit"));
                }
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data);
            }
        }
        Ok(events)
    }
    fn flush(&mut self) -> Result<Option<ChatEvent>, GatewayError> {
        if self.data.is_empty() {
            return Ok(None);
        }
        let data = std::mem::take(&mut self.data);
        if data == "[DONE]" {
            return Err(failure("ChatGPT stream ended without completed inference"));
        }
        let value: Value =
            serde_json::from_str(&data).map_err(|_| failure("ChatGPT SSE event is invalid"))?;
        match value["type"].as_str() {
            Some("response.output_text.delta" | "response.refusal.delta") => {
                let delta = value["delta"]
                    .as_str()
                    .ok_or_else(|| failure("ChatGPT text event is invalid"))?;
                if delta.is_empty() {
                    return Ok(None);
                }
                self.text_bytes = self.text_bytes.saturating_add(delta.len());
                if self.text_bytes > 8_388_608 {
                    return Err(failure("ChatGPT response exceeds the size limit"));
                }
                self.saw_text = true;
                Ok(Some(ChatEvent {
                    content: delta.into(),
                    done: false,
                }))
            }
            Some("response.completed") => {
                if value["response"]["status"] != "completed" || !self.saw_text {
                    return Err(failure("ChatGPT completed without a valid text response"));
                }
                self.terminal = true;
                Ok(Some(ChatEvent {
                    content: String::new(),
                    done: true,
                }))
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                let code = value
                    .pointer("/response/error/code")
                    .or_else(|| value.pointer("/error/code"))
                    .or_else(|| value.get("code"))
                    .and_then(Value::as_str);
                match code {
                    Some("subscription_sharing_usage_limit_exceeded") => Err(GatewayError::typed(
                        GatewayErrorKind::ProviderUsageLimited,
                        "ChatGPT plan usage limit reached",
                        false,
                    )),
                    Some(
                        "subscription_sharing_usage_unavailable"
                        | "subscription_sharing_user_unavailable",
                    ) => Err(GatewayError::backend_unavailable(
                        "ChatGPT plan usage temporarily unavailable",
                    )),
                    Some(
                        "subscription_sharing_user_not_eligible"
                        | "subscription_sharing_route_not_supported"
                        | "chatpass_v2_scope_not_authorized"
                        | "chatpass_v2_invalid_authorization_context",
                    ) => Err(GatewayError::typed(
                        GatewayErrorKind::ProviderAccessDenied,
                        "ChatGPT plan access denied",
                        false,
                    )),
                    Some("subscription_sharing_invalid_user") => Err(super::login_required()),
                    _ => Err(failure("ChatGPT inference did not complete")),
                }
            }
            _ => Ok(None),
        }
    }
    pub(super) fn completed(&self) -> bool {
        self.terminal
    }
    pub(super) fn finish(&self) -> Result<(), GatewayError> {
        if self.terminal {
            Ok(())
        } else {
            Err(failure("ChatGPT stream closed before response.completed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_network_boundaries_and_utf8_require_completed_event() {
        let wire="event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"héllo\"}\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
        for width in 1..wire.len() {
            let mut decoder = Decoder::default();
            let mut events = vec![];
            for chunk in wire.as_bytes().chunks(width) {
                events.extend(decoder.push(chunk).unwrap());
            }
            decoder.finish().unwrap();
            assert_eq!(
                events,
                vec![
                    ChatEvent {
                        content: "héllo".into(),
                        done: false
                    },
                    ChatEvent {
                        content: String::new(),
                        done: true
                    }
                ]
            );
        }
    }
    #[test]
    fn partial_text_eof_done_marker_and_failure_never_commit() {
        let text = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(text).unwrap().len(), 1);
        assert!(decoder.finish().is_err());
        assert!(decoder.push(b"data: [DONE]\n\n").is_err());
        for event in ["response.failed", "response.incomplete", "error"] {
            let mut decoder = Decoder::default();
            decoder.push(text).unwrap();
            let wire=format!("data: {{\"type\":\"{event}\",\"response\":{{\"error\":{{\"code\":\"subscription_sharing_usage_limit_exceeded\",\"message\":\"do not expose this secret\"}}}}}}\n\n");
            let error = decoder.push(wire.as_bytes()).unwrap_err();
            assert!(!error.message.contains("secret"));
            assert!(!decoder.completed());
        }
    }
    #[test]
    fn top_level_error_codes_keep_quota_policy_and_authentication_distinct() {
        for (code, kind) in [
            (
                "subscription_sharing_usage_limit_exceeded",
                GatewayErrorKind::ProviderUsageLimited,
            ),
            (
                "chatpass_v2_scope_not_authorized",
                GatewayErrorKind::ProviderAccessDenied,
            ),
            (
                "subscription_sharing_invalid_user",
                GatewayErrorKind::SessionExpired,
            ),
        ] {
            let wire=format!("data: {{\"type\":\"error\",\"code\":\"{code}\",\"message\":\"private fixture detail\"}}\n\n");
            let error = Decoder::default().push(wire.as_bytes()).unwrap_err();
            assert_eq!(error.kind, kind);
            assert!(!error.message.contains("private fixture"));
        }
    }
    #[test]
    fn oversized_and_malformed_events_fail_closed() {
        assert!(Decoder::default().push(&vec![b'x'; MAX_EVENT + 1]).is_err());
        assert!(Decoder::default().push(b"data: not json\n\n").is_err());
        assert!(Decoder::default().push(b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n").is_err());
    }
}
