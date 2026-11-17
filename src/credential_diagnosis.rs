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
    observed_at_ms: u64,
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
    uuid::Uuid::parse_str(raw).ok().filter(|id| !id.is_nil())
}

fn canonical_request_id(raw: &str) -> Option<uuid::Uuid> {
    let id = generation(raw)?;
    (id.to_string() == raw).then_some(id)
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
        e.gateway_failure.as_ref().map(|x| x.observed_at_ms),
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
        if failure.duration_ms > 120_000
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
            attempt
                .request_id
                .as_deref()
                .and_then(canonical_request_id),
        ) else {
            return inconclusive(provider, "missing_gateway_request_correlation");
        };
        if gateway_id != attempt_id {
            return inconclusive(provider, "gateway_request_id_mismatch");
        }
        if attempt.outcome != Outcome::Failed {
            return inconclusive(provider, "contradictory_gateway_failure_evidence");
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

    let chrome = e.chrome.as_ref().map(|x| x.verdict);
    let obscura = e.obscura.as_ref().map(|x| x.verdict);
    let snapshot = e.snapshot.as_ref().map(|x| x.state);

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
    fn gateway_failure_requires_matching_canonical_id_and_failed_attempt() {
        let mut v = with_gateway_failure();
        v["gateway_failure"]["request_id"] = json!(OTHER_ID);
        assert_eq!(checked(v.clone()).reason, "gateway_request_id_mismatch");
        v["gateway_failure"]["request_id"] = json!(ID);
        v["attempt"]["request_id"] = json!("COOKIE=secret");
        assert_eq!(checked(v.clone()).reason, "missing_gateway_request_correlation");
        v["attempt"]["request_id"] = json!(ID.to_uppercase());
        assert_eq!(checked(v.clone()).reason, "missing_gateway_request_correlation");
        v["attempt"]["request_id"] = json!(ID);
        v["gateway_failure"]["request_id"] = json!("00000000-0000-0000-0000-000000000000");
        assert_eq!(checked(v.clone()).reason, "missing_gateway_request_correlation");
        v["gateway_failure"]["request_id"] = json!(ID);
        v["attempt"].as_object_mut().unwrap().remove("request_id");
        assert_eq!(checked(v.clone()).reason, "missing_gateway_request_correlation");
        v["attempt"]["request_id"] = json!(ID);
        v.as_object_mut().unwrap().remove("attempt");
        assert_eq!(checked(v.clone()).reason, "missing_gateway_request_correlation");
        v["attempt"] = base()["attempt"].clone();
        v["attempt"]["request_id"] = json!(ID);
        v["attempt"]["outcome"] = json!("passed");
        assert_eq!(checked(v).reason, "contradictory_gateway_failure_evidence");
    }

    #[test]
    fn gateway_failure_stage_reason_duration_and_freshness_are_strict() {
        let mut v = with_gateway_failure();
        v["gateway_failure"]["stage"] = json!("https://private.example/?token=secret");
        assert_eq!(checked(v.clone()).reason, "invalid_gateway_failure_evidence");
        v["gateway_failure"]["stage"] = json!("first_stream_event");
        v["gateway_failure"]["reason"] = json!("private cookie value");
        assert_eq!(checked(v.clone()).reason, "invalid_gateway_failure_evidence");
        v["gateway_failure"]["reason"] = json!("timeout");
        v["gateway_failure"]["duration_ms"] = json!(120_001);
        assert_eq!(checked(v.clone()).reason, "invalid_gateway_failure_evidence");
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
