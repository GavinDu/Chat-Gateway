#!/usr/bin/env python3
"""Resolve explicit verification inputs without network or session access.

This preflight never starts a runtime or submits a prompt. Its result report
keeps every execution stage blocked until a separate executor observes it.
"""
from __future__ import annotations

import argparse
import datetime
import json
import re
import uuid
from pathlib import Path

PROVIDERS = (
    "chatgpt", "claude", "gemini", "grok", "metaai", "mistral",
    "perplexity", "qwen", "manus",
)
STAGES = (
    "1.capture", "1.persist_activate", "2.1.fetch", "2.2.fill",
    "2.3.submit", "2.4.extract", "2.5.envelope", "2.6.deliver", "recovery",
)
SCENARIOS = (
    "fresh_capture", "normal_replies", "memory_recall", "graceful_restart",
    "crash_recovery", "logout_relink", "refresh_while_busy", "fault_injection",
    "concurrency", "soak",
)


class InputError(Exception):
    """Fixed categories only; never attach input or filesystem error text."""


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise InputError("duplicate_input_field")
        result[key] = value
    return result


def load_inputs(path):
    try:
        with Path(path).open("rb") as stream:
            body = stream.read(65_537)
        if len(body) > 65_536:
            raise InputError("input_record_too_large")
        value = json.loads(body, object_pairs_hook=_unique_object,
                           parse_constant=lambda _: (_ for _ in ()).throw(
                               InputError("invalid_input_record")))
    except InputError:
        raise
    except (OSError, ValueError, UnicodeError, RecursionError):
        raise InputError("invalid_input_record") from None
    if not isinstance(value, dict):
        raise InputError("invalid_input_record")
    return value


def _get(record, path):
    value = record
    for field in path.split("."):
        if not isinstance(value, dict):
            return None
        value = value.get(field)
    return value


def resolve_inputs(record):
    for field in ("input_record_version", "standard_version"):
        if type(record.get(field)) is not int or record[field] != 1:
            raise InputError("unsupported_input_version")
    selected = _get(record, "selection.providers_and_models")
    if (not isinstance(selected, dict) or not selected
            or any(provider not in PROVIDERS for provider in selected)):
        raise InputError("invalid_provider_selection")
    missing = set()

    def require(path, predicate):
        if not predicate(_get(record, path)):
            missing.add(path)

    text = lambda value: isinstance(value, str) and bool(value.strip())
    positive = lambda value: type(value) is int and value > 0
    nonnegative = lambda value: type(value) is int and value >= 0
    for provider in selected:
        require("selection.providers_and_models." + provider, text)
    require("selection.outputs", lambda x: isinstance(x, list) and bool(x)
            and len(x) == len(set(y for y in x if isinstance(y, str)))
            and all(y in ("json", "sse") for y in x))
    require("selection.content_policy", text)
    for field in ("deployment_type", "gateway_origin", "build_choice",
                  "configuration_label", "resource_ownership_boundary"):
        require("target." + field, text)
    require("target.expected_commit", lambda x: isinstance(x, str)
            and re.fullmatch(r"[0-9a-f]{40}", x) is not None)
    require("target.image_digest_or_native_binary_digest", lambda x: isinstance(x, str)
            and re.fullmatch(r"sha256:[0-9a-f]{64}", x) is not None)
    require("target.attestation_plan", text)
    for field in ("source", "account_type", "local_authorization_method_label"):
        require("session." + field, text)
    for scenario in SCENARIOS:
        require("scenarios." + scenario, lambda x: type(x) is bool)
    require("permissions.live_requests", lambda x: x is True)
    if _get(record, "scenarios.fresh_capture") is True:
        require("session.chrome_available", lambda x: x is True)
        require("session.chrome_profile_label", text)
        require("session.extension_revision", text)
        require("session.source", lambda x: x == "fresh_capture")
    for scenario, permission in (
        ("graceful_restart", "restart_disposable_resources"),
        ("crash_recovery", "fault_injection"),
        ("logout_relink", "logout_relink"),
        ("fault_injection", "fault_injection"),
        ("concurrency", "load_testing"), ("soak", "load_testing"),
    ):
        if _get(record, "scenarios." + scenario) is True:
            require("permissions." + permission, lambda x: x is True)
    for field in ("maximum_requests_per_provider", "maximum_concurrency",
                  "queue_timeout_seconds", "maximum_duration_seconds"):
        require("limits." + field, positive)
    require("limits.minimum_interval_ms", nonnegative)
    require("limits.response_timeout_seconds", lambda x: positive(x)
            or (isinstance(x, dict) and set(x) == set(selected)
                and all(positive(y) for y in x.values())))
    for field in ("daily_or_cost_cap", "stop_conditions", "retry_before_dispatch_policy"):
        require("limits." + field, text)
    require("acceptance.mode", lambda x: x in ("baseline", "thresholds"))
    if _get(record, "acceptance.mode") == "thresholds":
        require("acceptance.minimum_success_rate", lambda x: type(x) in (float, int)
                and 0 < x <= 1)
        for field in ("queue_latency_target_ms", "first_token_latency_target_ms",
                      "total_latency_target_ms"):
            require("acceptance." + field, positive)
        require("acceptance.resource_stability_target", text)
    if _get(record, "scenarios.soak") is True:
        require("acceptance.sample_count", positive)
        require("acceptance.soak_duration_seconds", positive)
    for field in ("local_output_directory", "retention", "sharing"):
        require("evidence." + field, text)
    outputs = _get(record, "selection.outputs")
    needed = ((len(outputs) if isinstance(outputs, list) else 0)
              if _get(record, "scenarios.normal_replies") is True else 0)
    needed += 2 if _get(record, "scenarios.memory_recall") is True else 0
    budget = _get(record, "limits.maximum_requests_per_provider")
    if type(budget) is int and budget < needed:
        missing.add("limits.maximum_requests_per_provider")
    # This number covers normal replies and recall only. Lifecycle/load
    # executors must additionally cost their selected scenarios before sending.
    return {
        "plan_version": 1,
        "selected_providers": list(selected),
        "inputs_complete": not missing,
        "missing_inputs": sorted(missing),
        "normal_and_recall_requests_per_provider": needed,
        "live_requests_sent": 0,
    }


def blocked_report(record, plan):
    return {
        "schema_version": 1, "standard_version": 1,
        "run_id": str(uuid.uuid4()), "input_record_reference": "current_input_record",
        # Expected input identity is not an observation of a running binary.
        "gateway_commit": None, "runtime_digest": None,
        "extension_revision": None,
        "observed_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "cleanup_status": "not_applicable",
        "results": [{
            "provider": provider, "model_label": None, "scenario": "input_preflight",
            "stage": stage, "status": "blocked",
            "reason": "verification_not_started" if plan["inputs_complete"] else "run_inputs_incomplete",
            "duration_ms": None, "evidence_kind": None, "evidence_reference": None,
            "request_id": None, "submission_state": "not_applicable",
            "session_persistence": "unknown", "session_activation": "unknown",
            "session_durability": "unknown", "account_verdict": "unknown",
        } for provider in plan["selected_providers"] for stage in STAGES],
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", required=True)
    parser.add_argument("--report", help="Optional standard result JSON; never contains observed passes")
    args = parser.parse_args(argv)
    try:
        record = load_inputs(args.inputs)
        plan = resolve_inputs(record)
        if args.report:
            Path(args.report).write_text(json.dumps(blocked_report(record, plan), indent=2) + "\n")
    except InputError as error:
        print(json.dumps({"status": "blocked", "reason": str(error)}))
        return 2
    except OSError:
        print(json.dumps({"status": "blocked", "reason": "report_write_failed"}))
        return 2
    print(json.dumps(plan, indent=2))
    return 0 if plan["inputs_complete"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
