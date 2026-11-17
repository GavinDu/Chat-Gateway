//! Per-gateway provider session lifecycle state.
use crate::chat_backend::{GatewayError, GatewayErrorKind};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

/// Typed provider lifecycle state. Keep its string representation at API boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSessionState {
    Unknown,
    Ready,
    NeedsLogin,
    Degraded,
}

impl ProviderSessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ready => "ready",
            Self::NeedsLogin => "needs_login",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionStatus {
    pub state: ProviderSessionState,
    pub last_error: Option<String>,
    /// Earliest time a degraded provider may receive a half-open retry.
    pub retry_after: Option<SystemTime>,
}

const DEGRADED_RETRY_COOLDOWN: Duration = Duration::from_secs(15);
const CREDENTIAL_PROOF_TTL: Duration = Duration::from_secs(300);

const LAST_FAILED_REQUEST_TTL: Duration = Duration::from_secs(300);

/// Diagnostics for a gateway-generated request, not an upstream session.
/// Every serialized field is bounded or selected from a fixed source list.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LastFailedRequest {
    pub request_id: String,
    pub stage: &'static str,
    pub reason: &'static str,
    pub duration_ms: u64,
    pub observed_at_utc: String,
}

#[derive(Debug)]
struct RequestAttemptSlot {
    request_id: uuid::Uuid,
    last_failure: Option<(LastFailedRequest, Instant)>,
}

fn active_failure_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "chatgpt" => Some("chatgpt"),
        "metaai" => Some("metaai"),
        _ => None,
    }
}

fn safe_failure_stage(stage: &str) -> &'static str {
    match stage {
        "candidate_admission" => "candidate_admission",
        "backend_admission" => "backend_admission",
        "first_stream_event" => "first_stream_event",
        "json_completion" => "json_completion",
        _ => "unknown",
    }
}

fn safe_failure_reason(reason: &str) -> &'static str {
    match reason {
        "invalid_request" => "invalid_request",
        "model_not_found" => "model_not_found",
        "provider_busy" => "provider_busy",
        "session_expired" => "session_expired",
        "challenge" => "challenge",
        "timeout" => "timeout",
        "memory" => "memory",
        "provider_failure" => "provider_failure",
        "asset_blocked" => "asset_blocked",
        "document_denied" => "document_denied",
        "input_rejected" => "input_rejected",
        "backend_unavailable" => "backend_unavailable",
        "candidate_skipped" => "candidate_skipped",
        "unclassified" => "unclassified",
        _ => "unclassified",
    }
}


/// An opaque, non-credential-derived epoch for one provider/worker account
/// observation. No cookie hashes, raw account data or tokens are retained.
#[derive(Debug)]
struct CredentialProof {
    epoch: uuid::Uuid,
    first_party_verified_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct CredentialProofStore {
    by_provider: HashMap<String, CredentialProof>,
}
const MANUAL_VERIFICATION_NOTICE: &str =
    "provider manual verification required; resolve challenge and reset before retrying";

/// Cloneable session registry. Clones share one gateway's state; independent
/// registries are isolated, including in tests and embedded gateway instances.
#[derive(Clone, Debug, Default)]
pub struct ProviderSessionRegistry {
    states: Arc<Mutex<HashMap<String, ProviderSessionStatus>>>,
    credential_proofs: Arc<Mutex<CredentialProofStore>>,
    /// Two known active providers only; new requests fence older completions.
    last_request_attempts: Arc<Mutex<HashMap<&'static str, RequestAttemptSlot>>>,
}

impl ProviderSessionRegistry {
    /// Begin one owner-attributable request attempt before any candidate awaits.
    /// Generated request IDs are UUIDs; malformed or external strings cannot
    /// become diagnostic labels. Newer attempts hide stale prior failures.
    pub fn begin_request_attempt(&self, provider: &str, request_id: &str) {
        let Some(provider) = active_failure_provider(provider) else {
            return;
        };
        let Ok(request_id) = uuid::Uuid::parse_str(request_id) else {
            return;
        };
        if request_id.is_nil() {
            return;
        }
        if let Ok(mut slots) = self.last_request_attempts.lock() {
            slots.insert(
                provider,
                RequestAttemptSlot {
                    request_id,
                    last_failure: None,
                },
            );
        }
    }

    /// Record a failure only if the current provider slot still belongs to
    /// the same gateway request. Never retain upstream messages or response
    /// text: both stage and reason are mapped to static allowlisted strings.
    pub fn record_request_failure(
        &self,
        provider: &str,
        request_id: &str,
        stage: &str,
        reason: &str,
        duration_ms: u64,
    ) {
        let Some(provider) = active_failure_provider(provider) else {
            return;
        };
        let Ok(request_id) = uuid::Uuid::parse_str(request_id) else {
            return;
        };
        let Ok(mut slots) = self.last_request_attempts.lock() else {
            return;
        };
        let Some(slot) = slots.get_mut(provider) else {
            return;
        };
        if slot.request_id != request_id {
            return;
        }
        let record = LastFailedRequest {
            request_id: request_id.to_string(),
            stage: safe_failure_stage(stage),
            reason: safe_failure_reason(reason),
            duration_ms: duration_ms.min(120_000),
            observed_at_utc: chrono::Utc::now().to_rfc3339_opts(
                chrono::SecondsFormat::Secs,
                true,
            ),
        };
        slot.last_failure = Some((record, Instant::now()));
    }

    fn last_failed_request_at(&self, provider: &str, now: Instant) -> Option<LastFailedRequest> {
        let provider = active_failure_provider(provider)?;
        self.last_request_attempts.lock().ok().and_then(|slots| {
            slots.get(provider).and_then(|slot| {
                let (record, at) = slot.last_failure.as_ref()?;
                (now.checked_duration_since(*at)? <= LAST_FAILED_REQUEST_TTL)
                    .then(|| record.clone())
            })
        })
    }

    pub fn last_failed_request(&self, provider: &str) -> Option<LastFailedRequest> {
        self.last_failed_request_at(provider, Instant::now())
    }

    pub fn clear_last_request(&self, provider: &str) {
        let Some(provider) = active_failure_provider(provider) else {
            return;
        };
        if let Ok(mut slots) = self.last_request_attempts.lock() {
            slots.remove(provider);
        }
    }

    /// Invalidate a prior positive result before a new imported-session check.
    /// Only matching epochs may publish a later source-verified verdict.
    pub fn begin_credential_verification(&self, provider: &str) -> Option<uuid::Uuid> {
        let mut proofs = self.credential_proofs.lock().ok()?;
        let epoch = uuid::Uuid::new_v4();
        proofs.by_provider.insert(
            provider.to_owned(),
            CredentialProof {
                epoch,
                first_party_verified_at: None,
            },
        );
        Some(epoch)
    }

    pub fn record_credentials_verified(&self, provider: &str, epoch: uuid::Uuid) -> bool {
        let Ok(mut proofs) = self.credential_proofs.lock() else {
            return false;
        };
        let Some(current) = proofs.by_provider.get_mut(provider) else {
            return false;
        };
        if current.epoch != epoch {
            return false;
        }
        current.first_party_verified_at = Some(Instant::now());
        true
    }

    fn credentials_verified_at(&self, provider: &str, now: Instant) -> bool {
        self.credential_proofs
            .lock()
            .ok()
            .and_then(|proofs| {
                proofs
                    .by_provider
                    .get(provider)
                    .and_then(|entry| entry.first_party_verified_at)
            })
            .and_then(|observed| now.checked_duration_since(observed))
            .is_some_and(|age| age <= CREDENTIAL_PROOF_TTL)
    }

    pub fn credentials_verified(&self, provider: &str) -> bool {
        self.credentials_verified_at(provider, Instant::now())
    }

    pub fn invalidate_credentials(&self, provider: &str) {
        if let Ok(mut proofs) = self.credential_proofs.lock() {
            proofs.by_provider.remove(provider);
        }
    }

    /// Worker reinitialization must reject a late result from the old worker.
    /// Reuse of an old UUID across new epochs is not permitted.
    pub fn invalidate_all_credentials(&self) {
        if let Ok(mut proofs) = self.credential_proofs.lock() {
            proofs.by_provider.clear();
        }
    }

    pub fn status(&self, provider: &str) -> ProviderSessionStatus {
        self.states
            .lock()
            .ok()
            .and_then(|map| map.get(provider).cloned())
            .unwrap_or(ProviderSessionStatus {
                state: ProviderSessionState::Unknown,
                last_error: None,
                retry_after: None,
            })
    }

    /// Mark a provider as requiring authentication after its session is unlinked.
    pub fn record_unlinked(&self, provider: &str) {
        self.invalidate_credentials(provider);
        self.clear_last_request(provider);
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: ProviderSessionState::NeedsLogin,
                    last_error: Some("provider session unlinked; re-login required".to_owned()),
                    retry_after: None,
                },
            );
        }
    }

    pub fn record_needs_login(&self, provider: &str) {
        self.invalidate_credentials(provider);
        self.record_error_state(provider, true, None);
    }

    /// Expose only a fixed, non-sensitive classification for dashboards.
    /// Manual challenges never become automatic cooldown retries.
    pub fn manual_challenge_required(&self, provider: &str) -> bool {
        let status = self.status(provider);
        status.state == ProviderSessionState::Degraded
            && status.retry_after.is_none()
            && status.last_error.as_deref() == Some(MANUAL_VERIFICATION_NOTICE)
    }

    pub fn record_probe_failure(&self, provider: &str) {
        self.record_error_state(
            provider,
            false,
            SystemTime::now().checked_add(DEGRADED_RETRY_COOLDOWN),
        );
    }

    /// An operator reset allows the next request to probe the provider again;
    /// it does not claim that a browser session has been authenticated.
    pub fn reset_for_retry(&self, provider: &str) -> anyhow::Result<()> {
        self.states
            .lock()
            .map_err(|_| anyhow::anyhow!("provider state unavailable"))?
            .remove(provider);
        self.invalidate_credentials(provider);
        self.clear_last_request(provider);
        Ok(())
    }

    /// New credentials permit a login retry, but do not erase a failed chat
    /// merely because the extension rotated unrelated provider cookies.
    pub fn record_session_import(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            if map
                .get(provider)
                .is_some_and(|status| status.state == ProviderSessionState::Degraded)
            {
                return;
            }
            map.remove(provider);
        }
    }

    /// A visible composer cannot clear a failed chat attempt or explicit unlink.
    pub fn record_probe_success(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            let status = map
                .entry(provider.to_owned())
                .or_insert(ProviderSessionStatus {
                    state: ProviderSessionState::Ready,
                    last_error: None,
                    retry_after: None,
                });
            if status.state == ProviderSessionState::Unknown {
                status.state = ProviderSessionState::Ready;
            }
        }
    }

    pub fn record_success(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: ProviderSessionState::Ready,
                    last_error: None,
                    retry_after: None,
                },
            );
        }
    }

    /// Classify canonical backend errors by type, never by upstream text.
    /// Busy/caller/memory errors cannot make an otherwise healthy linked
    /// provider appear logged out. Only a typed authentication failure can
    /// force re-login; other provider failures enter a bounded cooldown.
    pub fn record_gateway_error(&self, provider: &str, error: &GatewayError) {
        match error.kind {
            GatewayErrorKind::SessionExpired => self.record_needs_login(provider),
            GatewayErrorKind::Challenge => self.record_manual_challenge(provider),
            GatewayErrorKind::InvalidRequest
            | GatewayErrorKind::ModelNotFound
            | GatewayErrorKind::ProviderBusy
            | GatewayErrorKind::Memory => {}
            GatewayErrorKind::ProviderAssetBlocked => self.record_asset_blocked(provider),
            GatewayErrorKind::ProviderDocumentDenied => self.record_document_denied(provider),
            GatewayErrorKind::ProviderInputRejected => self.record_input_rejected(provider),
            GatewayErrorKind::Timeout
            | GatewayErrorKind::ProviderFailure
            | GatewayErrorKind::BackendUnavailable => self.record_probe_failure(provider),
        }
    }

    /// Legacy untyped diagnostics must never force a provider into NeedsLogin.
    /// Provider-controlled text is untrusted; only a typed backend error may
    /// assert that authentication expired or a challenge was detected.
    pub fn record_error(&self, provider: &str, _error: &str) {
        self.record_probe_failure(provider);
    }

    /// A challenge requires owner action, but does not prove the session
    /// credentials expired. Fail closed without a scheduled half-open retry.
    /// New relay cookie imports cannot silently clear this manual blocker.
    fn record_manual_challenge(&self, provider: &str) {
        self.invalidate_credentials(provider);
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: ProviderSessionState::Degraded,
                    last_error: Some(MANUAL_VERIFICATION_NOTICE.to_owned()),
                    retry_after: None,
                },
            );
        }
    }

    fn record_asset_blocked(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: ProviderSessionState::Degraded,
                    last_error: Some(
                        "provider JavaScript assets blocked (HTTP 403); check network and browser compatibility"
                            .to_owned(),
                    ),
                    retry_after: SystemTime::now().checked_add(DEGRADED_RETRY_COOLDOWN),
                },
            );
        }
    }

    /// A first-party provider document 403 blocks bootstrap, but is not proof
    /// of session expiry. Never store an upstream URL or response body.
    fn record_document_denied(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: ProviderSessionState::Degraded,
                    last_error: Some(
                        "provider document denied (HTTP 403); check network and browser access"
                            .to_owned(),
                    ),
                    retry_after: SystemTime::now().checked_add(DEGRADED_RETRY_COOLDOWN),
                },
            );
        }
    }

    /// Editor-input incompatibility is neither a successful send nor proof of
    /// authentication expiry. Preserve a stable, non-secret diagnostic.
    fn record_input_rejected(&self, provider: &str) {
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: ProviderSessionState::Degraded,
                    last_error: Some(
                        "provider composer rejected input; check embedded browser compatibility"
                            .to_owned(),
                    ),
                    retry_after: SystemTime::now().checked_add(DEGRADED_RETRY_COOLDOWN),
                },
            );
        }
    }

    fn record_error_state(&self, provider: &str, expired: bool, retry_after: Option<SystemTime>) {
        if let Ok(mut map) = self.states.lock() {
            map.insert(
                provider.to_owned(),
                ProviderSessionStatus {
                    state: if expired {
                        ProviderSessionState::NeedsLogin
                    } else {
                        ProviderSessionState::Degraded
                    },
                    // Never retain upstream error text: it can contain credentials or PII.
                    last_error: Some(if expired {
                        "provider session expired; re-login required".to_owned()
                    } else {
                        "provider request failed; retry may succeed".to_owned()
                    }),
                    retry_after,
                },
            );
        }
    }

    /// Non-mutating readiness check; it does not claim the half-open retry slot.
    pub fn is_available(&self, provider: &str) -> bool {
        let now = SystemTime::now();
        let Ok(map) = self.states.lock() else {
            return false;
        };
        let Some(current) = map.get(provider) else {
            return true;
        };
        match current.state {
            ProviderSessionState::NeedsLogin => false,
            ProviderSessionState::Degraded => current
                .retry_after
                .is_some_and(|retry_after| now >= retry_after),
            ProviderSessionState::Unknown | ProviderSessionState::Ready => true,
        }
    }

    /// Admit only one half-open retry after a degraded cooldown.
    pub fn is_routable(&self, provider: &str) -> bool {
        let now = SystemTime::now();
        let Ok(mut map) = self.states.lock() else {
            return false;
        };
        let Some(current) = map.get_mut(provider) else {
            return true;
        };
        match current.state {
            ProviderSessionState::NeedsLogin => false,
            ProviderSessionState::Degraded => {
                if current
                    .retry_after
                    .is_some_and(|retry_after| now >= retry_after)
                {
                    current.retry_after = now.checked_add(DEGRADED_RETRY_COOLDOWN);
                    true
                } else {
                    false
                }
            }
            ProviderSessionState::Unknown | ProviderSessionState::Ready => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_expiry_is_typed_redacted_and_cleared_on_success() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "provider",
            &GatewayError::session_expired("cookie=secret; user=private@example.test"),
        );
        let expired = registry.status("provider");
        assert_eq!(expired.state, ProviderSessionState::NeedsLogin);
        assert_eq!(
            expired.last_error.as_deref(),
            Some("provider session expired; re-login required")
        );
        assert_eq!(expired.retry_after, None);
        registry.record_success("provider");
        assert_eq!(
            registry.status("provider").state,
            ProviderSessionState::Ready
        );
        assert_eq!(registry.status("provider").last_error, None);
    }

    #[test]
    fn untyped_provider_message_cannot_force_reauthentication() {
        let registry = ProviderSessionRegistry::default();
        registry.record_error(
            "provider",
            "session expired, please sign in; cookie=secret; user=private@example.test",
        );
        let status = registry.status("provider");
        assert_eq!(status.state, ProviderSessionState::Degraded);
        assert!(status.retry_after.is_some());
        assert_eq!(
            status.last_error.as_deref(),
            Some("provider request failed; retry may succeed")
        );
    }

    #[test]
    fn observed_asset_403_is_actionable_degraded_not_expired_or_unsanitized() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "mistral",
            &GatewayError::typed(
                GatewayErrorKind::ProviderAssetBlocked,
                "cookie=private; upstream script query=?token=secret",
                false,
            ),
        );
        let status = registry.status("mistral");
        assert_eq!(status.state, ProviderSessionState::Degraded);
        assert!(status.retry_after.is_some());
        let message = status.last_error.expect("safe asset failure status");
        assert!(message.contains("JavaScript assets blocked (HTTP 403)"));
        assert!(!message.contains("private"));
        assert!(!message.contains("token"));
    }

    #[test]
    fn metaai_document_denial_is_degraded_not_expired_and_never_retains_upstream_text() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "metaai",
            &GatewayError::typed(
                GatewayErrorKind::ProviderDocumentDenied,
                "document?token=secret; cookie=private",
                false,
            ),
        );
        let status = registry.status("metaai");
        assert_eq!(status.state, ProviderSessionState::Degraded);
        assert!(status.retry_after.is_some());
        let message = status.last_error.expect("fixed document denial category");
        assert!(message.contains("document denied (HTTP 403)"));
        assert!(!message.contains("secret"));
        assert!(!message.contains("private"));
        assert!(!registry.manual_challenge_required("metaai"));
    }

    #[test]
    fn rejected_composer_input_is_actionable_but_never_claims_auth_expiry() {
        let registry = ProviderSessionRegistry::default();
        registry.record_success("chatgpt");
        registry.record_gateway_error(
            "chatgpt",
            &GatewayError::typed(
                GatewayErrorKind::ProviderInputRejected,
                "prompt=secret-value; cookie=private",
                false,
            ),
        );
        let status = registry.status("chatgpt");
        assert_eq!(status.state, ProviderSessionState::Degraded);
        assert!(status.retry_after.is_some());
        assert_eq!(
            status.last_error.as_deref(),
            Some("provider composer rejected input; check embedded browser compatibility")
        );
        assert!(!registry.manual_challenge_required("chatgpt"));
        registry.record_success("chatgpt");
        assert_eq!(
            registry.status("chatgpt").state,
            ProviderSessionState::Ready
        );
    }

    #[test]
    fn typed_errors_never_infer_auth_expiry_from_untrusted_messages() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "false-expiry",
            &GatewayError::provider_failure("upstream said session expired: cookie=secret-value"),
        );
        let degraded = registry.status("false-expiry");
        assert_eq!(degraded.state, ProviderSessionState::Degraded);
        assert!(degraded.retry_after.is_some());
        assert!(!degraded.last_error.unwrap().contains("secret-value"));

        registry.record_gateway_error(
            "auth-expired",
            &GatewayError::session_expired("arbitrary sensitive upstream text"),
        );
        let expired = registry.status("auth-expired");
        assert_eq!(expired.state, ProviderSessionState::NeedsLogin);
        assert_eq!(expired.retry_after, None);

        registry.record_gateway_error(
            "challenge",
            &GatewayError::challenge("challenge from provider"),
        );
        let challenged = registry.status("challenge");
        assert_eq!(challenged.state, ProviderSessionState::Degraded);
        assert_eq!(challenged.retry_after, None);
        assert!(!registry.is_available("challenge"));
    }

    #[test]
    fn manual_challenge_is_not_false_session_expiry_or_automatic_retry() {
        let registry = ProviderSessionRegistry::default();
        registry.record_success("gemini");
        registry.record_gateway_error(
            "gemini",
            &GatewayError::challenge("cookie=private; bearer=secret"),
        );
        let challenged = registry.status("gemini");
        assert_eq!(challenged.state, ProviderSessionState::Degraded);
        assert_eq!(challenged.retry_after, None);
        let message = challenged.last_error.as_deref().unwrap();
        assert!(message.contains("manual verification"));
        assert!(!message.contains("private"));
        assert!(!message.contains("secret"));
        assert!(!message.contains("expired"));
        assert!(!registry.is_routable("gemini"));
        assert!(!registry.is_available("gemini"));
        assert!(registry.manual_challenge_required("gemini"));

        registry.record_probe_success("gemini");
        registry.record_session_import("gemini");
        assert_eq!(registry.status("gemini"), challenged);
        registry.reset_for_retry("gemini").unwrap();
        assert_eq!(
            registry.status("gemini").state,
            ProviderSessionState::Unknown
        );
        assert!(!registry.manual_challenge_required("gemini"));
    }

    #[test]
    fn busy_caller_and_memory_errors_do_not_degrade_a_healthy_session() {
        let registry = ProviderSessionRegistry::default();
        let provider = "healthy";
        registry.record_success(provider);
        for error in [
            GatewayError::provider_busy("busy"),
            GatewayError::typed(GatewayErrorKind::InvalidRequest, "bad request", false),
            GatewayError::typed(GatewayErrorKind::ModelNotFound, "unknown model", false),
            GatewayError::typed(GatewayErrorKind::Memory, "store unavailable", false),
        ] {
            registry.record_gateway_error(provider, &error);
            assert_eq!(registry.status(provider).state, ProviderSessionState::Ready);
        }
    }

    #[test]
    fn transient_errors_enter_cooldown_and_expired_sessions_never_half_open() {
        let registry = ProviderSessionRegistry::default();
        registry.record_error("transient", "temporary network timeout");
        assert_eq!(
            registry.status("transient").state,
            ProviderSessionState::Degraded
        );
        assert!(!registry.is_routable("transient"));
        registry.record_needs_login("expired");
        assert!(!registry.is_routable("expired"));
        assert!(!registry.is_available("expired"));
    }

    #[test]
    fn readiness_does_not_claim_half_open_and_only_one_request_can_claim_it() {
        let registry = ProviderSessionRegistry::default();
        registry.record_error_state("provider", false, Some(SystemTime::UNIX_EPOCH));
        assert!(registry.is_available("provider"));
        assert!(registry.is_available("provider"));
        assert!(registry.is_routable("provider"));
        assert!(!registry.is_routable("provider"));
    }

    #[test]
    fn cloned_registry_shares_state_but_independent_instances_are_isolated() {
        let first = ProviderSessionRegistry::default();
        let clone = first.clone();
        let second = ProviderSessionRegistry::default();
        first.record_success("provider");
        assert_eq!(clone.status("provider").state, ProviderSessionState::Ready);
        assert_eq!(
            second.status("provider").state,
            ProviderSessionState::Unknown
        );
    }

    #[test]
    fn admin_retry_reset_does_not_falsely_claim_authenticated_session() {
        let registry = ProviderSessionRegistry::default();
        registry.record_needs_login("provider");
        assert!(!registry.is_routable("provider"));
        registry.reset_for_retry("provider").unwrap();
        assert_eq!(
            registry.status("provider").state,
            ProviderSessionState::Unknown
        );
        assert!(registry.is_routable("provider"));
        assert!(registry.status("provider").last_error.is_none());
    }

    #[test]
    fn unlinking_session_requires_reauthentication() {
        let registry = ProviderSessionRegistry::default();
        registry.record_success("provider");
        registry.record_unlinked("provider");
        assert_eq!(
            registry.status("provider").state,
            ProviderSessionState::NeedsLogin
        );
        assert!(!registry.is_routable("provider"));
    }

    #[test]
    fn state_string_conversion_is_stable() {
        assert_eq!(ProviderSessionState::Unknown.as_str(), "unknown");
        assert_eq!(ProviderSessionState::Ready.as_str(), "ready");
        assert_eq!(ProviderSessionState::NeedsLogin.as_str(), "needs_login");
        assert_eq!(ProviderSessionState::Degraded.as_str(), "degraded");
    }
}

#[cfg(test)]
mod probe_outcome_tests {
    use super::*;
    #[test]
    fn composer_probe_cannot_erase_chat_failure_or_explicit_unlink() {
        let registry = ProviderSessionRegistry::default();
        registry.record_gateway_error(
            "chatgpt",
            &GatewayError::typed(GatewayErrorKind::ProviderFailure, "synthetic", true),
        );
        let failed = registry.status("chatgpt");
        registry.record_probe_success("chatgpt");
        registry.record_session_import("chatgpt");
        assert_eq!(registry.status("chatgpt"), failed);
        registry.record_unlinked("grok");
        registry.record_probe_success("grok");
        assert_eq!(
            registry.status("grok").state,
            ProviderSessionState::NeedsLogin
        );
        registry.record_success("chatgpt");
        assert_eq!(
            registry.status("chatgpt").state,
            ProviderSessionState::Ready
        );
    }
}

#[cfg(test)]
mod credential_evidence_lifecycle_tests {
    use super::*;

    #[test]
    fn evidence_requires_matching_epoch_and_expires_after_five_minutes() {
        let reg = ProviderSessionRegistry::default();
        assert!(!reg.credentials_verified("chatgpt"));
        let first = reg.begin_credential_verification("chatgpt").unwrap();
        assert!(!reg.credentials_verified("chatgpt"));
        assert!(reg.record_credentials_verified("chatgpt", first));
        assert!(reg.credentials_verified("chatgpt"));
        let expired_now = Instant::now() + CREDENTIAL_PROOF_TTL + Duration::from_secs(1);
        assert!(!reg.credentials_verified_at("chatgpt", expired_now));
        let second = reg.begin_credential_verification("chatgpt").unwrap();
        assert_ne!(first, second);
        assert!(!reg.record_credentials_verified("chatgpt", first));
        assert!(!reg.credentials_verified("chatgpt"));
        assert!(reg.record_credentials_verified("chatgpt", second));
        assert!(reg.credentials_verified("chatgpt"));
    }

    #[test]
    fn logout_restart_and_typed_expiry_revoke_previous_evidence() {
        let reg = ProviderSessionRegistry::default();
        let a = reg.begin_credential_verification("chatgpt").unwrap();
        assert!(reg.record_credentials_verified("chatgpt", a));
        reg.record_unlinked("chatgpt");
        assert!(!reg.credentials_verified("chatgpt"));
        assert!(!reg.record_credentials_verified("chatgpt", a));

        let b = reg.begin_credential_verification("chatgpt").unwrap();
        assert!(reg.record_credentials_verified("chatgpt", b));
        reg.record_needs_login("chatgpt");
        assert!(!reg.credentials_verified("chatgpt"));
        let c = reg.begin_credential_verification("chatgpt").unwrap();
        reg.invalidate_all_credentials();
        assert!(!reg.record_credentials_verified("chatgpt", c));
        assert!(!reg.credentials_verified("chatgpt"));
    }

    #[test]
    fn cloned_registry_and_provider_epochs_are_isolated() {
        let reg = ProviderSessionRegistry::default();
        let clone = reg.clone();
        let chat = reg.begin_credential_verification("chatgpt").unwrap();
        let meta = reg.begin_credential_verification("metaai").unwrap();
        assert!(clone.record_credentials_verified("chatgpt", chat));
        assert!(!clone.credentials_verified("metaai"));
        assert!(reg.record_credentials_verified("metaai", meta));
        reg.invalidate_credentials("chatgpt");
        assert!(!clone.credentials_verified("chatgpt"));
        assert!(clone.credentials_verified("metaai"));
        assert!(!ProviderSessionRegistry::default().credentials_verified("metaai"));
        assert!(!clone.record_credentials_verified("chatgpt", chat));
    }

    #[test]
    fn transient_non_authentication_failure_does_not_mislabel_credentials_expired() {
        let reg = ProviderSessionRegistry::default();
        let epoch = reg.begin_credential_verification("chatgpt").unwrap();
        assert!(reg.record_credentials_verified("chatgpt", epoch));
        reg.record_probe_failure("chatgpt");
        assert!(reg.credentials_verified("chatgpt"));
        reg.record_needs_login("chatgpt");
        assert!(!reg.credentials_verified("chatgpt"));
    }
}

#[cfg(test)]
mod bounded_request_diagnostic_tests {
    use super::*;

    const FIRST: &str = "5f855e0d-5b22-4a93-b925-98db841a1ee1";
    const SECOND: &str = "e7d97a2b-195b-43db-9c8e-9f8a0da3408d";

    #[test]
    fn older_completion_cannot_replace_latest_attempt() {
        let reg = ProviderSessionRegistry::default();
        reg.begin_request_attempt("chatgpt", FIRST);
        reg.begin_request_attempt("chatgpt", SECOND);
        reg.record_request_failure("chatgpt", FIRST, "json_completion", "timeout", 500);
        assert!(reg.last_failed_request("chatgpt").is_none());
        reg.record_request_failure(
            "chatgpt",
            SECOND,
            "first_stream_event",
            "backend_unavailable",
            123,
        );
        let latest = reg.last_failed_request("chatgpt").unwrap();
        assert_eq!(latest.request_id.to_string(), SECOND);
        assert_eq!(latest.stage, "first_stream_event");
        assert_eq!(latest.reason, "backend_unavailable");
        assert_eq!(latest.duration_ms, 123);
    }

    #[test]
    fn bounded_provider_cardinality_and_cross_provider_isolation() {
        let reg = ProviderSessionRegistry::default();
        reg.begin_request_attempt("metaai", FIRST);
        reg.record_request_failure("metaai", FIRST, "backend_admission", "timeout", 240_000);
        reg.begin_request_attempt("chatgpt", SECOND);
        reg.record_request_failure("chatgpt", SECOND, "candidate_admission", "provider_busy", 2);
        reg.begin_request_attempt("grok", FIRST);
        reg.record_request_failure("grok", FIRST, "candidate_admission", "provider_busy", 2);
        assert_eq!(reg.last_request_attempts.lock().unwrap().len(), 2);
        assert!(reg.last_failed_request("grok").is_none());
        assert_eq!(reg.last_failed_request("metaai").unwrap().duration_ms, 120_000);
        reg.clear_last_request("chatgpt");
        assert!(reg.last_failed_request("chatgpt").is_none());
        assert!(reg.last_failed_request("metaai").is_some());
    }

    #[test]
    fn unsafe_stage_reason_and_id_cannot_leak_tokens_or_urls() {
        let reg = ProviderSessionRegistry::default();
        reg.begin_request_attempt("chatgpt", "cookie=verysecret");
        assert!(reg.last_failed_request("chatgpt").is_none());
        reg.begin_request_attempt("chatgpt", FIRST);
        reg.record_request_failure(
            "chatgpt",
            FIRST,
            "https://private.example/credential",
            "bearer_secret_from_upstream",
            u64::MAX,
        );
        let record = reg.last_failed_request("chatgpt").unwrap();
        let serialized = serde_json::to_string(&record).unwrap();
        assert_eq!(record.stage, "unknown");
        assert_eq!(record.reason, "unclassified");
        assert_eq!(record.duration_ms, 120_000);
        for private in ["private.example", "bearer_secret", "credential", "verysecret"] {
            assert!(!serialized.contains(private));
        }
        assert_eq!(serde_json::to_value(&record).unwrap().as_object().unwrap().len(), 5);
    }

    #[test]
    fn no_failure_expiry_unlink_and_reset_do_not_fake_success() {
        let reg = ProviderSessionRegistry::default();
        reg.begin_request_attempt("chatgpt", FIRST);
        assert!(reg.last_failed_request("chatgpt").is_none());
        reg.record_request_failure("chatgpt", FIRST, "json_completion", "unclassified", 1);
        let expired = Instant::now() + LAST_FAILED_REQUEST_TTL + Duration::from_secs(1);
        assert!(reg.last_failed_request_at("chatgpt", expired).is_none());
        reg.record_unlinked("chatgpt");
        assert!(reg.last_failed_request("chatgpt").is_none());
        reg.begin_request_attempt("chatgpt", SECOND);
        reg.record_request_failure("chatgpt", SECOND, "backend_admission", "timeout", 1);
        reg.reset_for_retry("chatgpt").unwrap();
        assert!(reg.last_failed_request("chatgpt").is_none());
    }
}
