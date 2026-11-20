//! Offline, strictly redacted diagnosis of owner-supplied evidence.
//!
//! This does NOT verify credentials, load the vault or contact providers. Only
//! source-verified account checks may legitimately create "verified" evidence.
use serde::Deserialize;
use std::{
    fs,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_FILE: u64 = 16 * 1024;
const MAX_AGE_MS: u64 = 5 * 60 * 1000;
// The observed gateway wall clock may exceed its 120s request deadline by a
// bounded scheduling/serialization tail; elapsed time is not failure-phase proof.
const MAX_GATEWAY_FAILURE_DURATION_MS: u64 = 180_000;
const CLOCK_SKEW_MS: u64 = 30 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Auth {
    Verified,
    Unauthenticated,
    Unknown,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotState {
    Missing,
    Rejected,
    SavedActivationPending,
    Applied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Unknown,
    Bootstrap,
    Composer,
    Send,
    Response,
    Stream,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Failed,
    Passed,
    Unknown,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountEvidence {
    verdict: Auth,
    observed_at_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayEvidence {
    verdict: Auth,
    observed_at_ms: u64,
    generation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotEvidence {
    state: SnapshotState,
    observed_at_ms: u64,
    generation: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptEvidence {
    stage: Stage,
    outcome: Outcome,
    observed_at_ms: u64,
    generation: String,
    /// Optional gateway-generated request UUID; not a session fingerprint.
    request_id: Option<String>,
}

/// Optional owner-status last-failure observation, not a proof of account
/// validity or an Obscura internal DOM/transport subphase.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayFailureEvidence {
    request_id: String,
    stage: String,
    reason: String,
    duration_ms: u64,
    /// Fixture-compatible Unix milliseconds; do not accept together with UTC.
    observed_at_ms: Option<u64>,
    /// Exact owner status API format, not a freeform provider timestamp.
    observed_at_utc: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    provider: String,
    chrome: Option<AccountEvidence>,
    snapshot: Option<SnapshotEvidence>,
    obscura: Option<ReplayEvidence>,
    attempt: Option<AttemptEvidence>,
    gateway_failure: Option<GatewayFailureEvidence>,
}

#[derive(Debug, serde::Serialize)]
struct Diagnosis {
    schema_version: u8,
    provider: &'static str,
    category: &'static str,
    reason: &'static str,
    next_action: &'static str,
    /// This tool never runs a live check; supplied evidence is a claim only.
    live_proof: bool,
}

fn verdict(
    provider: &'static str,
    category: &'static str,
    reason: &'static str,
    next_action: &'static str,
) -> Diagnosis {
    Diagnosis {
        schema_version: 1,
        provider,
        category,
        reason,
        next_action,
        live_proof: false,
    }
}

fn inconclusive(provider: &'static str, reason: &'static str) -> Diagnosis {
    verdict(
        provider,
        "inconclusive",
        reason,
        "collect_current_correlated_evidence",
    )
}

fn fresh(at: u64, now: u64) -> bool {
    at <= now.saturating_add(CLOCK_SKEW_MS) && now.saturating_sub(at) <= MAX_AGE_MS
}

fn generation(raw: &str) -> Option<uuid::Uuid> {
    let id = uuid::Uuid::parse_str(raw).ok().filter(|id| !id.is_nil())?;
    // Snapshot, imported-page and attempt generations are opaque source
    // identifiers. Do not normalize unsupported owner-supplied spellings into
    // a seemingly correlated credential/replay record.
    (id.to_string() == raw).then_some(id)
}

fn canonical_request_id(raw: &str) -> Option<uuid::Uuid> {
    generation(raw)
}

/// Only a single source-owned timestamp shape may establish recency. Direct
/// owner-status records use canonical RFC3339 UTC seconds; older offline
/// fixtures may still provide Unix milliseconds. A contradictory pair, missing
/// field, invalid date/zone, or overflowing value supplies no usable evidence.
fn gateway_failure_timestamp(e: &GatewayFailureEvidence) -> Option<u64> {
    match (e.observed_at_ms, e.observed_at_utc.as_deref()) {
        (Some(ms), None) => Some(ms),
        (None, Some(utc)) => {
            if utc.len() != 20 || !utc.ends_with('Z') {
                return None;
            }
            let time = chrono::DateTime::parse_from_rfc3339(utc).ok()?;
            if time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) != utc {
                return None;
            }
            u64::try_from(time.timestamp_millis()).ok()
        }
        _ => None,
    }
}

fn fixed_gateway_stage(stage: &str) -> bool {
    matches!(
        stage,
        "candidate_admission" | "backend_admission" | "first_stream_event" | "json_completion"
    )
}

fn fixed_gateway_reason(reason: &str) -> bool {
    matches!(
        reason,
        "invalid_request"
            | "model_not_found"
            | "provider_busy"
            | "session_expired"
            | "challenge"
            | "timeout"
            | "memory"
            | "provider_failure"
            | "asset_blocked"
            | "document_denied"
            | "input_rejected"
            | "backend_unavailable"
            | "candidate_skipped"
            | "unclassified"
    )
}

fn classify(e: &Evidence, now: u64) -> Diagnosis {
    let provider = match e.provider.as_str() {
        "chatgpt" => "chatgpt",
        "metaai" => "metaai",
        _ => {
            return verdict(
                "unsupported",
                "inconclusive",
                "unsupported_provider",
                "use_chatgpt_or_metaai",
            )
        }
    };
    let timestamps = [
        e.chrome.as_ref().map(|x| x.observed_at_ms),
        e.snapshot.as_ref().map(|x| x.observed_at_ms),
        e.obscura.as_ref().map(|x| x.observed_at_ms),
        e.attempt.as_ref().map(|x| x.observed_at_ms),
        e.gateway_failure
            .as_ref()
            .and_then(gateway_failure_timestamp),
    ];
    if timestamps
        .into_iter()
        .flatten()
        .any(|time| !fresh(time, now))
    {
        return inconclusive(provider, "stale_or_future_evidence");
    }

    // A cross-generation report is never evidence about the same credentials.
    let ids = [
        e.snapshot.as_ref().and_then(|x| x.generation.as_deref()),
        e.obscura.as_ref().map(|x| x.generation.as_str()),
        e.attempt.as_ref().map(|x| x.generation.as_str()),
    ];
    let parsed = ids
        .into_iter()
        .flatten()
        .map(generation)
        .collect::<Vec<_>>();
    if parsed.iter().any(Option::is_none) || parsed.windows(2).any(|pair| pair[0] != pair[1]) {
        return inconclusive(provider, "snapshot_generation_mismatch");
    }
    if let Some(failure) = e.gateway_failure.as_ref() {
        if gateway_failure_timestamp(failure).is_none()
            || failure.duration_ms > MAX_GATEWAY_FAILURE_DURATION_MS
            || !fixed_gateway_stage(&failure.stage)
            || !fixed_gateway_reason(&failure.reason)
        {
            return inconclusive(provider, "invalid_gateway_failure_evidence");
        }
        let Some(attempt) = e.attempt.as_ref() else {
            return inconclusive(provider, "missing_gateway_request_correlation");
        };
        let (Some(gateway_id), Some(attempt_id)) = (
            canonical_request_id(&failure.request_id),
            attempt.request_id.as_deref().and_then(canonical_request_id),
        ) else {
            return inconclusive(provider, "missing_gateway_request_correlation");
        };
        if gateway_id != attempt_id {
            return inconclusive(provider, "gateway_request_id_mismatch");
        }
        if attempt.outcome != Outcome::Failed {
            return inconclusive(provider, "contradictory_gateway_failure_evidence");
        }
        // Owner status is observed after the failed request, never before it.
        // A matching UUID alone cannot make contradictory clock order causal.
        if gateway_failure_timestamp(failure)
            .is_some_and(|observed_at| observed_at < attempt.observed_at_ms)
        {
            return inconclusive(provider, "gateway_failure_precedes_attempt");
        }
        // Candidate/backend admission failed before provider work started.
        // It cannot establish an adapter, composer, Send, or response-phase
        // failure on this *same* request. Joining fallback attempt evidence
        // by a shared request UUID must fail closed instead of assigning blame.
        if matches!(
            failure.stage.as_str(),
            "candidate_admission" | "backend_admission"
        ) && attempt.stage != Stage::Unknown
        {
            return inconclusive(provider, "conflicting_gateway_failure_stage");
        }
        // Preserve the producer's stage/reason relationship. These pairs
        // cannot be emitted by chat_handler; accepting them as independent
        // fixed labels would let contradictory offline evidence blame a
        // different request phase. Existing admission/attempt conflict wins
        // above, so this additional check cannot mask that stronger evidence.
        if (failure.stage == "candidate_admission"
            && !matches!(
                failure.reason.as_str(),
                "provider_busy" | "candidate_skipped"
            ))
            || (failure.stage != "candidate_admission" && failure.reason == "candidate_skipped")
        {
            return inconclusive(provider, "inconsistent_gateway_stage_reason");
        }
    }
    if (e.snapshot.is_none() && (e.obscura.is_some() || e.attempt.is_some()))
        || (e.snapshot.as_ref().is_some_and(|snapshot| {
            matches!(
                snapshot.state,
                SnapshotState::Applied | SnapshotState::SavedActivationPending
            ) && snapshot.generation.is_none()
        }))
    {
        return inconclusive(provider, "missing_snapshot_correlation");
    }

    // A later browser login, new imported snapshot, or successful Obscura
    // account observation cannot retroactively establish authentication when
    // an earlier request failed. Freshness and matching opaque generation
    // alone are insufficient to assert causal request-phase attribution.
    // Gateway failure status is recorded *after* a request; it is not an
    // account prerequisite and is separately checked by request UUID above.
    if let Some(attempt) = e.attempt.as_ref() {
        let verified_prerequisites = [
            e.chrome.as_ref().map(|proof| proof.observed_at_ms),
            e.snapshot.as_ref().map(|state| state.observed_at_ms),
            e.obscura.as_ref().map(|proof| proof.observed_at_ms),
        ];
        if verified_prerequisites
            .into_iter()
            .flatten()
            .any(|time| time > attempt.observed_at_ms)
        {
            return inconclusive(provider, "post_attempt_evidence");
        }
    }

    let chrome = e.chrome.as_ref().map(|x| x.verdict);
    let obscura = e.obscura.as_ref().map(|x| x.verdict);
    let snapshot = e.snapshot.as_ref().map(|x| x.state);

    // Meta AI has no source-established first-party account-verifier contract.
    // A claimed positive (or explicit rejection) from manually supplied evidence
    // cannot turn a failed request into an Obscura blame or re-login verdict.
    // Remove this fence only when #1329 ships a source-bound verifier.
    if provider == "metaai"
        && [chrome, obscura]
            .into_iter()
            .flatten()
            .any(|auth| matches!(auth, Auth::Verified | Auth::Unauthenticated))
    {
        return inconclusive(provider, "metaai_account_verifier_not_established");
    }

    // A first-party account observation collected before the supplied applied
    // snapshot checkpoint does not prove account state *on that checkpoint*.
    // These are observation times, not an engine-owned application timestamp;
    // fail conservatively rather than claim credential replay or downstream
    // adapter/transport attribution from a potentially earlier page state.
    if let (Some(snapshot), Some(obscura)) = (&e.snapshot, &e.obscura) {
        if snapshot.state == SnapshotState::Applied
            && matches!(obscura.verdict, Auth::Verified | Auth::Unauthenticated)
            && obscura.observed_at_ms < snapshot.observed_at_ms
        {
            return inconclusive(
                provider,
                "account_proof_predates_applied_snapshot_observation",
            );
        }
    }

    if chrome == Some(Auth::Unauthenticated) && obscura == Some(Auth::Verified) {
        return inconclusive(provider, "contradictory_account_observations");
    }
    if chrome == Some(Auth::Unauthenticated) {
        return verdict(
            provider,
            "browser_login_attention",
            "chrome_explicitly_unauthenticated",
            "verify_login_on_official_website",
        );
    }
    if matches!(
        snapshot,
        Some(SnapshotState::Missing | SnapshotState::Rejected)
    ) {
        return verdict(
            provider,
            "capture_or_import",
            "snapshot_missing_or_rejected",
            "inspect_capture_and_relay_without_forced_resync",
        );
    }
    if snapshot == Some(SnapshotState::SavedActivationPending) {
        return verdict(
            provider,
            "activation_pending",
            "snapshot_saved_but_browser_not_activated",
            "inspect_activation_generation",
        );
    }
    if snapshot != Some(SnapshotState::Applied) {
        return inconclusive(provider, "applied_snapshot_unproven");
    }
    if chrome == Some(Auth::Verified) && obscura == Some(Auth::Unauthenticated) {
        return verdict(
            provider,
            "replay_or_binding",
            "different_account_verdict_on_applied_snapshot",
            "compare_same_snapshot_scope_and_context",
        );
    }
    if obscura != Some(Auth::Verified) {
        return inconclusive(provider, "authoritative_obscura_account_verifier_missing");
    }
    if e.gateway_failure
        .as_ref()
        .is_some_and(|failure| matches!(failure.reason.as_str(), "session_expired" | "challenge"))
    {
        // A former first-party positive observation can be superseded by an
        // explicit new login/challenge signal. Recheck current session state
        // rather than mislabel the later failure as a transport defect.
        return inconclusive(provider, "account_or_challenge_state_changed");
    }
    match e.attempt.as_ref().map(|a| (a.stage, a.outcome)) {
        Some((Stage::Composer | Stage::Send, Outcome::Failed)) => verdict(
            provider,
            "adapter_input",
            "authenticated_composer_or_send_failed",
            "inspect_editor_and_prompt_acknowledgement",
        ),
        Some((Stage::Response | Stage::Stream, Outcome::Failed)) => verdict(
            provider,
            "transport_or_extraction",
            "authenticated_send_response_failed",
            "inspect_transport_and_current_turn_response",
        ),
        Some((Stage::Completed, Outcome::Passed)) => {
            inconclusive(provider, "no_failure_in_supplied_evidence")
        }
        _ => inconclusive(provider, "request_phase_or_terminal_evidence_missing"),
    }
}

pub(crate) fn diagnose_file(path: &Path) -> anyhow::Result<String> {
    // Read-only local operator input. Reject symlinks and oversized files;
    // do not reflect filesystem or parser errors with credential-bearing data.
    let info = fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("credential evidence unavailable"))?;
    if !info.is_file() || info.file_type().is_symlink() || info.len() > MAX_FILE {
        anyhow::bail!("credential evidence unavailable");
    }
    // Bound reads even when an operator-managed file changes after lstat.
    // Never allocate or parse an unbounded credential-bearing JSON document.
    let mut reader = fs::File::open(path)
        .map_err(|_| anyhow::anyhow!("credential evidence unavailable"))?
        .take(MAX_FILE + 1);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("credential evidence unavailable"))?;
    if bytes.len() as u64 > MAX_FILE {
        anyhow::bail!("credential evidence unavailable");
    }
    let e: Evidence = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("credential evidence invalid"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    serde_json::to_string(&classify(&e, now))
        .map_err(|_| anyhow::anyhow!("credential diagnosis unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const ID: &str = "d9f06fa4-62f1-4b9c-8718-96ac8e553d16";
    const OTHER_ID: &str = "6fc111b5-3cca-471c-843e-c5669a236d91";
    const NOW: u64 = 2_000_000_000_000;

    fn base() -> Value {
        json!({
            "provider":"chatgpt",
            "chrome":{"verdict":"verified","observed_at_ms":NOW},
            "snapshot":{"state":"applied","observed_at_ms":NOW,"generation":ID},
            "obscura":{"verdict":"verified","observed_at_ms":NOW,"generation":ID},
            "attempt":{"stage":"response","outcome":"failed","observed_at_ms":NOW,"generation":ID}
        })
    }

    fn checked(v: Value) -> Diagnosis {
        classify(&serde_json::from_value(v).unwrap(), NOW)
    }

    #[test]
    fn exact_supported_evidence_classifies_adapter_and_transport() {
        let mut v = base();
        assert_eq!(checked(v.clone()).category, "transport_or_extraction");
        v["attempt"]["stage"] = json!("composer");
        assert_eq!(checked(v.clone()).category, "adapter_input");
        v["attempt"]["stage"] = json!("send");
        assert_eq!(checked(v.clone()).category, "adapter_input");
        v["attempt"]["outcome"] = json!("passed");
        assert_eq!(checked(v).category, "inconclusive");
    }

    fn with_gateway_failure() -> Value {
        let mut v = base();
        v["attempt"]["request_id"] = json!(ID);
        v["gateway_failure"] = json!({
            "request_id": ID,
            "stage": "json_completion",
            "reason": "backend_unavailable",
            "duration_ms": 120_000,
            "observed_at_ms": NOW
        });
        v
    }

    #[test]
    fn gateway_candidate_stage_reason_pairs_must_match_the_source_emitter() {
        let mut v = with_gateway_failure();
        v["attempt"]["stage"] = json!("unknown");
        v["gateway_failure"]["stage"] = json!("candidate_admission");
        for valid in ["provider_busy", "candidate_skipped"] {
            v["gateway_failure"]["reason"] = json!(valid);
            let result = checked(v.clone());
            assert_eq!(result.reason, "request_phase_or_terminal_evidence_missing");
            assert!(!result.live_proof);
        }
        for invalid in ["backend_unavailable", "session_expired", "unclassified"] {
            v["gateway_failure"]["reason"] = json!(invalid);
            let result = checked(v.clone());
            assert_eq!(result.reason, "inconsistent_gateway_stage_reason");
            assert!(!result.live_proof);
        }

        v["attempt"]["stage"] = json!("response");
        for stage in ["backend_admission", "first_stream_event", "json_completion"] {
            v["gateway_failure"]["stage"] = json!(stage);
            v["gateway_failure"]["reason"] = json!("candidate_skipped");
            let result = checked(v.clone());
            let expected = if stage == "backend_admission" {
                "conflicting_gateway_failure_stage"
            } else {
                "inconsistent_gateway_stage_reason"
            };
            assert_eq!(result.reason, expected);
            let serialized = serde_json::to_string(&result).unwrap();
            assert!(!serialized.contains(ID));
            assert!(!serialized.contains("candidate_skipped"));
            assert!(!result.live_proof);
        }

        // Legacy synthetic JSON completion fixtures retain their existing behavior.
        let normal = checked(with_gateway_failure());
        assert_eq!(normal.category, "transport_or_extraction");
        assert!(!normal.live_proof);
    }

    #[test]
    fn direct_owner_status_utc_timestamp_joins_only_when_canonical_and_fresh() {
        let mut v = with_gateway_failure();
        v["gateway_failure"]
            .as_object_mut()
            .unwrap()
            .remove("observed_at_ms");
        let timestamp = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(NOW as i64)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        v["gateway_failure"]["observed_at_utc"] = json!(timestamp);
        let result = checked(v.clone());
        assert_eq!(result.category, "transport_or_extraction");
        assert!(!result.live_proof);
        for invalid in [
            "2026-02-30T03:00:00Z",
            "2026-11-17T24:00:00Z",
            "2026-11-17T03:00:00+00:00",
            "2026-11-17T03:00:00.000Z",
            "https://private.example/?token=secret",
            "2026-11-17T03:00:00Z\nCOOKIE=secret",
        ] {
            v["gateway_failure"]["observed_at_utc"] = json!(invalid);
            assert_eq!(
                checked(v.clone()).reason,
                "invalid_gateway_failure_evidence"
            );
        }
    }

    #[test]
    fn contradictory_or_missing_gateway_clock_and_stale_utc_fail_closed() {
        let mut v = with_gateway_failure();
        let timestamp = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(NOW as i64)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        // Both timestamp representations are not a trusted pair; do not
        // guess which one describes the actual owner-status request.
        v["gateway_failure"]["observed_at_utc"] = json!(timestamp);
        assert_eq!(
            checked(v.clone()).reason,
            "invalid_gateway_failure_evidence"
        );
        v["gateway_failure"]
            .as_object_mut()
            .unwrap()
            .remove("observed_at_ms");
        v["gateway_failure"]
            .as_object_mut()
            .unwrap()
            .remove("observed_at_utc");
        assert_eq!(
            checked(v.clone()).reason,
            "invalid_gateway_failure_evidence"
        );
        let stale = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
            (NOW - MAX_AGE_MS - 1000) as i64,
        )
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        v["gateway_failure"]["observed_at_utc"] = json!(stale);
        assert_eq!(checked(v.clone()).reason, "stale_or_future_evidence");
        let future = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
            (NOW + CLOCK_SKEW_MS + 1000) as i64,
        )
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        v["gateway_failure"]["observed_at_utc"] = json!(future);
        assert_eq!(checked(v).reason, "stale_or_future_evidence");
    }

    #[test]
    fn matching_gateway_last_failure_is_only_owner_supplied_evidence_not_live_proof() {
        let v = with_gateway_failure();
        let result = checked(v);
        assert_eq!(result.category, "transport_or_extraction");
        assert!(!result.live_proof);
        let body = serde_json::to_string(&result).unwrap();
        assert!(!body.contains(ID));
        assert!(!body.contains("backend_unavailable"));
    }

    #[test]
    fn gateway_last_failure_must_follow_the_failed_attempt() {
        let mut evidence = with_gateway_failure();
        evidence["attempt"]["observed_at_ms"] = json!(NOW);
        evidence["gateway_failure"]["observed_at_ms"] = json!(NOW - 1);
        let early = checked(evidence.clone());
        assert_eq!(early.category, "inconclusive");
        assert_eq!(early.reason, "gateway_failure_precedes_attempt");
        assert!(!early.live_proof);
        let safe = serde_json::to_string(&early).unwrap();
        assert!(!safe.contains(ID));
        assert!(!safe.contains("token"));
        assert!(!safe.contains("cookie"));

        evidence["gateway_failure"]["observed_at_ms"] = json!(NOW);
        assert_eq!(
            checked(evidence.clone()).category,
            "transport_or_extraction"
        );
        evidence["gateway_failure"]["observed_at_ms"] = json!(NOW + 1_000);
        assert_eq!(
            checked(evidence.clone()).category,
            "transport_or_extraction"
        );

        evidence["provider"] = json!("metaai");
        evidence["chrome"]["verdict"] = json!("unsupported");
        evidence["obscura"]["verdict"] = json!("unknown");
        assert_eq!(
            checked(evidence.clone()).reason,
            "authoritative_obscura_account_verifier_missing"
        );

        // The UTC-only owner-status timestamp has identical ordering rules.
        evidence["provider"] = json!("chatgpt");
        evidence["chrome"]["verdict"] = json!("verified");
        evidence["obscura"]["verdict"] = json!("verified");
        evidence["gateway_failure"]
            .as_object_mut()
            .unwrap()
            .remove("observed_at_ms");
        let prior_utc =
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis((NOW - 1_000) as i64)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        evidence["gateway_failure"]["observed_at_utc"] = json!(prior_utc);
        assert_eq!(
            checked(evidence.clone()).reason,
            "gateway_failure_precedes_attempt"
        );
        let current_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(NOW as i64)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        evidence["gateway_failure"]["observed_at_utc"] = json!(current_utc);
        assert_eq!(
            checked(evidence.clone()).category,
            "transport_or_extraction"
        );

        // Request identity remains a mandatory separate source fence.
        evidence["gateway_failure"]["request_id"] = json!(OTHER_ID);
        assert_eq!(checked(evidence).reason, "gateway_request_id_mismatch");
    }

    #[test]
    fn gateway_failure_requires_matching_canonical_id_and_failed_attempt() {
        let mut v = with_gateway_failure();
        v["gateway_failure"]["request_id"] = json!(OTHER_ID);
        assert_eq!(checked(v.clone()).reason, "gateway_request_id_mismatch");
        v["gateway_failure"]["request_id"] = json!(ID);
        v["attempt"]["request_id"] = json!("COOKIE=secret");
        assert_eq!(
            checked(v.clone()).reason,
            "missing_gateway_request_correlation"
        );
        v["attempt"]["request_id"] = json!(ID.to_uppercase());
        assert_eq!(
            checked(v.clone()).reason,
            "missing_gateway_request_correlation"
        );
        v["attempt"]["request_id"] = json!(ID);
        v["gateway_failure"]["request_id"] = json!("00000000-0000-0000-0000-000000000000");
        assert_eq!(
            checked(v.clone()).reason,
            "missing_gateway_request_correlation"
        );
        v["gateway_failure"]["request_id"] = json!(ID);
        v["attempt"].as_object_mut().unwrap().remove("request_id");
        assert_eq!(
            checked(v.clone()).reason,
            "missing_gateway_request_correlation"
        );
        v["attempt"]["request_id"] = json!(ID);
        v.as_object_mut().unwrap().remove("attempt");
        assert_eq!(
            checked(v.clone()).reason,
            "missing_gateway_request_correlation"
        );
        v["attempt"] = base()["attempt"].clone();
        v["attempt"]["request_id"] = json!(ID);
        v["attempt"]["outcome"] = json!("passed");
        assert_eq!(checked(v).reason, "contradictory_gateway_failure_evidence");
    }

    #[test]
    fn real_120_second_timeout_tail_is_valid_evidence_not_account_proof() {
        // Meta AI's observed 120.030s installed failure is just above the
        // upstream 120s deadline; this cannot alone imply expired credentials.
        let mut v = with_gateway_failure();
        v["gateway_failure"]["duration_ms"] = json!(120_030);
        assert_eq!(checked(v.clone()).category, "transport_or_extraction");
        assert!(!checked(v.clone()).live_proof);

        v["provider"] = json!("metaai");
        v["chrome"]["verdict"] = json!("unsupported");
        v["obscura"]["verdict"] = json!("unknown");
        let no_account_proof = checked(v.clone());
        assert_eq!(no_account_proof.category, "inconclusive");
        assert_eq!(
            no_account_proof.reason,
            "authoritative_obscura_account_verifier_missing"
        );
        assert!(!no_account_proof.live_proof);

        v["gateway_failure"]["request_id"] = json!(OTHER_ID);
        assert_eq!(checked(v.clone()).reason, "gateway_request_id_mismatch");
        v["gateway_failure"]["request_id"] = json!(ID);
        v["snapshot"]["generation"] = json!(OTHER_ID);
        assert_eq!(checked(v.clone()).reason, "snapshot_generation_mismatch");
        v["snapshot"]["generation"] = json!(ID);
        v["gateway_failure"]["duration_ms"] = json!(MAX_GATEWAY_FAILURE_DURATION_MS + 1);
        assert_eq!(
            checked(v.clone()).reason,
            "invalid_gateway_failure_evidence"
        );

        let safe = serde_json::to_string(&checked(v)).unwrap();
        assert!(!safe.contains(ID));
        assert!(!safe.contains(OTHER_ID));
        assert!(!safe.contains("token"));
        assert!(!safe.contains("cookie"));
    }

    #[test]
    fn gateway_failure_stage_reason_duration_and_freshness_are_strict() {
        let mut v = with_gateway_failure();
        v["gateway_failure"]["stage"] = json!("https://private.example/?token=secret");
        assert_eq!(
            checked(v.clone()).reason,
            "invalid_gateway_failure_evidence"
        );
        v["gateway_failure"]["stage"] = json!("first_stream_event");
        v["gateway_failure"]["reason"] = json!("private cookie value");
        assert_eq!(
            checked(v.clone()).reason,
            "invalid_gateway_failure_evidence"
        );
        v["gateway_failure"]["reason"] = json!("timeout");
        v["gateway_failure"]["duration_ms"] = json!(MAX_GATEWAY_FAILURE_DURATION_MS + 1);
        assert_eq!(
            checked(v.clone()).reason,
            "invalid_gateway_failure_evidence"
        );
        v["gateway_failure"]["duration_ms"] = json!(90);
        v["gateway_failure"]["observed_at_ms"] = json!(NOW - MAX_AGE_MS - 1);
        assert_eq!(checked(v.clone()).reason, "stale_or_future_evidence");
        v["gateway_failure"]["observed_at_ms"] = json!(NOW + CLOCK_SKEW_MS + 1);
        assert_eq!(checked(v.clone()).reason, "stale_or_future_evidence");
        v["gateway_failure"]["observed_at_ms"] = json!(NOW);
        v["gateway_failure"]["token"] = json!("private-account-cookie");
        assert!(serde_json::from_value::<Evidence>(v).is_err());
    }

    #[test]
    fn coarse_admission_and_changed_account_states_cannot_be_transport_proof() {
        let mut v = with_gateway_failure();
        v["gateway_failure"]["stage"] = json!("backend_admission");
        assert_eq!(
            checked(v.clone()).reason,
            "conflicting_gateway_failure_stage"
        );
        v["gateway_failure"]["stage"] = json!("json_completion");
        v["gateway_failure"]["reason"] = json!("session_expired");
        assert_eq!(
            checked(v.clone()).reason,
            "account_or_challenge_state_changed"
        );
        v["gateway_failure"]["reason"] = json!("challenge");
        assert_eq!(checked(v).reason, "account_or_challenge_state_changed");
    }

    #[test]
    fn coarse_gateway_admission_never_certifies_later_provider_attempt_phase() {
        let mut evidence = with_gateway_failure();
        for gateway_stage in ["candidate_admission", "backend_admission"] {
            evidence["gateway_failure"]["stage"] = json!(gateway_stage);
            for attempted_phase in [
                "bootstrap",
                "composer",
                "send",
                "response",
                "stream",
                "completed",
            ] {
                evidence["attempt"]["stage"] = json!(attempted_phase);
                let decision = checked(evidence.clone());
                assert_eq!(
                    decision.category, "inconclusive",
                    "{gateway_stage}/{attempted_phase}"
                );
                assert_eq!(
                    decision.reason, "conflicting_gateway_failure_stage",
                    "{gateway_stage}/{attempted_phase}"
                );
                assert!(!decision.live_proof);
                let safe = serde_json::to_string(&decision).unwrap();
                for secret in [ID, "private", "cookie", "token"] {
                    assert!(!safe.contains(secret), "{gateway_stage}/{attempted_phase}");
                }
            }
            evidence["attempt"]["stage"] = json!("unknown");
            assert_eq!(
                checked(evidence.clone()).reason,
                "request_phase_or_terminal_evidence_missing"
            );
        }
        // The later gateway reporting checkpoints are not proof that a
        // provider response actually started. Preserve their existing
        // redacted adapter-input verdict on a source-verified failed attempt.
        for gateway_stage in ["first_stream_event", "json_completion"] {
            evidence["gateway_failure"]["stage"] = json!(gateway_stage);
            evidence["attempt"]["stage"] = json!("composer");
            assert_eq!(checked(evidence.clone()).category, "adapter_input");
        }
        // Canonical request identity is still a separate mandatory fence.
        evidence["gateway_failure"]["request_id"] = json!(OTHER_ID);
        assert_eq!(checked(evidence).reason, "gateway_request_id_mismatch");
    }

    #[test]
    fn gateway_failure_cannot_bypass_snapshot_generation_or_account_verification() {
        let mut v = with_gateway_failure();
        v["snapshot"]["generation"] = json!(OTHER_ID);
        assert_eq!(checked(v.clone()).reason, "snapshot_generation_mismatch");
        v["snapshot"]["generation"] = json!(ID);
        v["obscura"]["verdict"] = json!("unknown");
        assert_eq!(
            checked(v).reason,
            "authoritative_obscura_account_verifier_missing"
        );
    }

    #[test]
    fn explicit_browser_rejection_and_capture_paths_are_separate() {
        let mut v = base();
        v["chrome"]["verdict"] = json!("unauthenticated");
        assert_eq!(checked(v.clone()).category, "inconclusive");
        v["obscura"]["verdict"] = json!("unknown");
        assert_eq!(checked(v.clone()).category, "browser_login_attention");
        v["chrome"]["verdict"] = json!("verified");
        v["snapshot"]["state"] = json!("missing");
        assert_eq!(checked(v.clone()).category, "capture_or_import");
        v["snapshot"]["state"] = json!("rejected");
        assert_eq!(checked(v.clone()).category, "capture_or_import");
        v["snapshot"]["state"] = json!("saved_activation_pending");
        assert_eq!(checked(v).category, "activation_pending");
    }

    #[test]
    fn request_diagnosis_rejects_auth_and_import_evidence_recorded_after_failure() {
        // Every observation is individually fresh and uses the same source
        // generation. Later login/import/replay still cannot prove what
        // happened when the earlier request failed.
        for source in ["chrome", "snapshot", "obscura"] {
            let mut evidence = base();
            evidence[source]["observed_at_ms"] = json!(NOW + 1_000);
            let decision = checked(evidence);
            assert_eq!(decision.category, "inconclusive", "{source}");
            assert_eq!(decision.reason, "post_attempt_evidence", "{source}");
            assert!(!decision.live_proof);
        }

        // The same guard applies to a replay-versus-browser discrepancy:
        // later Chrome positivity cannot establish the earlier jar state.
        let mut mismatch = base();
        mismatch["obscura"]["verdict"] = json!("unauthenticated");
        mismatch["chrome"]["observed_at_ms"] = json!(NOW + 1_000);
        assert_eq!(checked(mismatch).reason, "post_attempt_evidence");
    }

    #[test]
    fn account_proof_predating_applied_snapshot_observation_cannot_assign_runtime_blame() {
        let mut evidence = base();
        evidence["obscura"]["observed_at_ms"] = json!(NOW - 1_000);
        let decision = checked(evidence.clone());
        assert_eq!(decision.category, "inconclusive");
        assert_eq!(
            decision.reason,
            "account_proof_predates_applied_snapshot_observation"
        );
        assert!(!decision.live_proof);
        assert!(!serde_json::to_string(&decision).unwrap().contains(ID));

        // Negative account evidence observed before the applied checkpoint
        // likewise cannot establish a replay/binding mismatch.
        evidence["obscura"]["verdict"] = json!("unauthenticated");
        assert_eq!(
            checked(evidence.clone()).reason,
            "account_proof_predates_applied_snapshot_observation"
        );
        // Meta AI has no authoritative account verifier at all; that
        // unsupported verdict must remain the stronger diagnosis regardless
        // of how owner-supplied observation times are ordered.
        let mut meta = base();
        meta["provider"] = json!("metaai");
        meta["obscura"]["observed_at_ms"] = json!(NOW - 1_000);
        assert_eq!(
            checked(meta).reason,
            "metaai_account_verifier_not_established"
        );

        // Equal-time checkpoints remain eligible for existing typed logic.
        evidence["obscura"]["observed_at_ms"] = json!(NOW);
        assert_eq!(checked(evidence).category, "replay_or_binding");
    }

    #[test]
    fn account_proof_checkpoint_order_must_hold_with_correlated_gateway_failure() {
        let mut evidence = with_gateway_failure();
        evidence["obscura"]["observed_at_ms"] = json!(NOW - 1_000);
        assert_eq!(
            checked(evidence.clone()).reason,
            "account_proof_predates_applied_snapshot_observation"
        );
        // A properly ordered, same-generation earlier snapshot can support
        // source-attributed diagnosis; the gateway UUID remains mandatory.
        evidence["snapshot"]["observed_at_ms"] = json!(NOW - 2_000);
        assert_eq!(
            checked(evidence.clone()).category,
            "transport_or_extraction"
        );
        evidence["attempt"]["request_id"] = json!(OTHER_ID);
        assert_eq!(checked(evidence).reason, "gateway_request_id_mismatch");
    }

    #[test]
    fn request_diagnosis_keeps_prior_proof_and_later_correlated_status_distinct() {
        let mut evidence = base();
        evidence["chrome"]["observed_at_ms"] = json!(NOW - 10_000);
        evidence["snapshot"]["observed_at_ms"] = json!(NOW - 9_000);
        evidence["obscura"]["observed_at_ms"] = json!(NOW - 1_000);
        assert_eq!(checked(evidence).category, "transport_or_extraction");

        let mut status = with_gateway_failure();
        status["gateway_failure"]["observed_at_ms"] = json!(NOW + 2_000);
        let decision = checked(status);
        assert_eq!(decision.category, "transport_or_extraction");
        assert!(!decision.live_proof);
        let serialized = serde_json::to_string(&decision).unwrap();
        assert!(!serialized.contains(ID));
        assert!(!serialized.contains("token=private"));
    }

    #[test]
    fn paired_mismatch_is_not_a_proven_obscura_engine_defect() {
        let mut v = base();
        v["obscura"]["verdict"] = json!("unauthenticated");
        let result = checked(v);
        assert_eq!(result.category, "replay_or_binding");
        assert_eq!(
            result.next_action,
            "compare_same_snapshot_scope_and_context"
        );
    }

    #[test]
    fn unknown_metaai_and_incomplete_success_cannot_claim_authentication() {
        let mut v = base();
        v["provider"] = json!("metaai");
        v["chrome"]["verdict"] = json!("unsupported");
        v["obscura"]["verdict"] = json!("unknown");
        assert_eq!(checked(v.clone()).category, "inconclusive");
        v["chrome"]["verdict"] = json!("verified");
        v["obscura"]["verdict"] = json!("verified");
        v["attempt"]["stage"] = json!("completed");
        v["attempt"]["outcome"] = json!("passed");
        let result = checked(v);
        assert_eq!(result.category, "inconclusive");
        assert!(!result.live_proof);
    }

    #[test]
    fn unsupported_metaai_account_claims_cannot_assign_login_or_runtime_blame() {
        let mut v = base();
        v["provider"] = json!("metaai");

        // A manual JSON claim is not a supported account verifier. No
        // Meta AI endpoint/response schema has been established by #1329.
        for context in ["chrome", "obscura"] {
            for claim in ["verified", "unauthenticated"] {
                v["chrome"]["verdict"] = json!("unknown");
                v["obscura"]["verdict"] = json!("unknown");
                v[context]["verdict"] = json!(claim);
                for stage in ["composer", "send", "response", "stream"] {
                    v["attempt"]["stage"] = json!(stage);
                    let result = checked(v.clone());
                    assert_eq!(result.category, "inconclusive", "{context}/{claim}/{stage}");
                    assert_eq!(result.reason, "metaai_account_verifier_not_established");
                    assert_eq!(result.next_action, "collect_current_correlated_evidence");
                    assert!(!result.live_proof);
                    let body = serde_json::to_string(&result).unwrap();
                    assert!(!body.contains(ID));
                    assert!(!body.contains("cookie"));
                }
            }
        }

        // With unknown/unsupported account evidence, independent capture
        // failures can still be reported without inventing login status.
        v["chrome"]["verdict"] = json!("unsupported");
        v["obscura"]["verdict"] = json!("unknown");
        v["snapshot"]["state"] = json!("missing");
        assert_eq!(checked(v.clone()).category, "capture_or_import");
        v["snapshot"]["state"] = json!("applied");
        assert_eq!(
            checked(v).reason,
            "authoritative_obscura_account_verifier_missing"
        );

        // Do not regress the supported ChatGPT diagnosis classification.
        let chatgpt = base();
        assert_eq!(checked(chatgpt.clone()).category, "transport_or_extraction");
        let mut chatgpt = chatgpt;
        chatgpt["chrome"]["verdict"] = json!("unauthenticated");
        chatgpt["obscura"]["verdict"] = json!("unknown");
        assert_eq!(checked(chatgpt).category, "browser_login_attention");
    }

    #[test]
    fn stale_mismatch_and_untrusted_fields_fail_closed() {
        let mut v = base();
        v["obscura"]["generation"] = json!(OTHER_ID);
        assert_eq!(checked(v.clone()).reason, "snapshot_generation_mismatch");
        v["obscura"]["generation"] = json!(ID);
        v["chrome"]["observed_at_ms"] = json!(NOW - MAX_AGE_MS - 1);
        assert_eq!(checked(v.clone()).reason, "stale_or_future_evidence");
        v["chrome"]["observed_at_ms"] = json!(NOW);
        v["snapshot"]["generation"] = json!("cookie=private");
        assert_eq!(checked(v.clone()).reason, "snapshot_generation_mismatch");
        v["snapshot"]["generation"] = json!(ID);
        v["secret_token"] = json!("bearer highly-secret");
        assert!(serde_json::from_value::<Evidence>(v).is_err());
    }

    #[test]
    fn snapshot_generation_requires_exact_canonical_spelling_in_each_context() {
        let invalid = [
            ID.to_uppercase(),
            format!("{{{ID}}}"),
            format!("urn:uuid:{ID}"),
            format!(" {ID}"),
            format!("{ID} "),
            "00000000-0000-0000-0000-000000000000".to_owned(),
            "https://private.example/?cookie=secret".to_owned(),
        ];
        for context in ["snapshot", "obscura", "attempt"] {
            for raw in &invalid {
                let mut v = base();
                v[context]["generation"] = json!(raw);
                let result = checked(v);
                assert_eq!(
                    result.reason, "snapshot_generation_mismatch",
                    "{context} must not normalize supplied generation"
                );
                assert_eq!(result.category, "inconclusive");
                assert!(!result.live_proof);
                let body = serde_json::to_string(&result).unwrap();
                assert!(!body.contains("secret"));
                assert!(!body.contains("private.example"));
            }
        }
        // Only a truly identical source-canonical generation may correlate.
        assert_eq!(checked(base()).category, "transport_or_extraction");
    }

    #[test]
    fn applied_snapshot_without_generation_never_diagnoses_runtime() {
        let mut value = base();
        value["snapshot"]["generation"] = serde_json::Value::Null;
        let result = checked(value);
        assert_eq!(result.category, "inconclusive");
        assert_eq!(result.reason, "missing_snapshot_correlation");
    }

    #[test]
    fn all_output_fields_are_fixed_and_no_secrets_escape() {
        let mut v = base();
        v["provider"] = json!("attacker.com/private?token=secret");
        let result = checked(v);
        let body = serde_json::to_string(&result).unwrap();
        assert_eq!(result.category, "inconclusive");
        assert!(!body.contains("attacker.com"));
        assert!(!body.contains("token=secret"));
        assert!(!body.contains(ID));
        assert!(!body.contains("bearer"));
    }
}
