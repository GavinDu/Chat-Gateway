#!/usr/bin/env python3
"""Opt-in JSON-first installed gateway smoke matrix.

By default only ChatGPT and Meta AI are tested; six other providers are
explicitly deferred until the owner changes the selection. The report always
contains all eight provider rows.

No owner session is accessed unless --execute is supplied. The output is
redacted metadata only: never print response bodies, prompts, model IDs,
authentication material, conversation IDs or arbitrary upstream errors.
This checks one fresh JSON turn, then a fresh SSE turn per provider; it does
not certify restart or memory continuity.
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import re
import time
import urllib.error
import urllib.request
import uuid

from live_provider_canary import (
    MAX_SSE_CONTENT_BYTES, MAX_SSE_STREAM_BYTES, _NoRedirect,
    bounded_sse_lines, consume_sse, gateway_headers, validate_canary_target,
)

PROVIDERS = (
    "chatgpt", "metaai", "gemini", "claude", "grok", "qwen",
    "perplexity", "mistral",
)
FOCUS_PROVIDERS = ("chatgpt", "metaai")
PROMPT = "Reply exactly OK"
MAX_JSON_BYTES = 262_144
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")
FIXED_CATEGORIES = frozenset({
    "invalid_json", "wrong_provider", "wrong_conversation",
    "missing_request_id", "unexpected_answer", "incomplete_finish",
    "sse_incomplete", "invalid_models", "gateway_not_ready",
    "gateway_unreachable", "response_too_large", "invalid_response",
    "invalid_header", "invalid_gateway_url", "invalid_selection",
})


class SmokeFailure(Exception):
    """A fixed safe code; upstream text must never enter this exception."""

    def __init__(self, category: str):
        self.category = category
        super().__init__(category)


def _request_id(headers) -> str | None:
    raw = headers.get("X-Request-Id")
    if not isinstance(raw, str):
        return None
    try:
        return str(uuid.UUID(raw))
    except (ValueError, AttributeError):
        return None


def _read_limited(response, maximum=MAX_JSON_BYTES) -> bytes:
    body = response.read(maximum + 1)
    if len(body) > maximum:
        raise SmokeFailure("response_too_large")
    return body


def _json_document(blob: bytes) -> dict:
    try:
        parsed = json.loads(blob.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        raise SmokeFailure("invalid_json") from None
    if not isinstance(parsed, dict):
        raise SmokeFailure("invalid_response")
    return parsed


def _safe_error(error: Exception) -> str:
    if isinstance(error, SmokeFailure):
        return error.category if error.category in FIXED_CATEGORIES else "invalid_response"
    if isinstance(error, urllib.error.HTTPError):
        code = error.code
        return f"http_{code}" if isinstance(code, int) and 400 <= code <= 599 else "http_error"
    if isinstance(error, (urllib.error.URLError, TimeoutError, OSError)):
        return "gateway_unreachable"
    return "invalid_response"


def _identity(health: dict) -> dict:
    # Never reflect arbitrary server fields, freeform strings, or URLs.
    out = {"health": "ok" if health.get("status") == "ok" else "unverified"}
    for path in (("build", "source_sha"), ("source_sha",), ("commit_sha",)):
        value = health
        for key in path:
            value = value.get(key) if isinstance(value, dict) else None
        if isinstance(value, str) and SHA_PATTERN.fullmatch(value):
            out["source_sha"] = value
            break
    return out


def model_inventory(models: dict) -> dict[str, str]:
    items = models.get("data")
    if not isinstance(items, list) or len(items) > 1024:
        raise SmokeFailure("invalid_models")
    found = {}
    # Model IDs are never included in the metadata report.
    for item in items:
        if not isinstance(item, dict):
            continue
        owner, model = item.get("owned_by"), item.get("id")
        if (owner in PROVIDERS and isinstance(model, str)
                and 0 < len(model) <= 256 and owner not in found):
            found[owner] = model
    return found


def validate_json(payload: dict, headers, provider: str, conversation_id: str) -> None:
    if headers.get("X-Gateway-Provider", "").lower() != provider:
        raise SmokeFailure("wrong_provider")
    if headers.get("X-Conversation-Id") != conversation_id:
        raise SmokeFailure("wrong_conversation")
    if _request_id(headers) is None:
        raise SmokeFailure("missing_request_id")
    choices = payload.get("choices")
    if not isinstance(choices, list) or len(choices) != 1:
        raise SmokeFailure("invalid_response")
    choice = choices[0]
    if not isinstance(choice, dict) or choice.get("finish_reason") != "stop":
        raise SmokeFailure("incomplete_finish")
    message = choice.get("message")
    if not isinstance(message, dict) or message.get("role") != "assistant":
        raise SmokeFailure("invalid_response")
    # A different or stale response must never pass; only exact marker text.
    if message.get("content") != "OK":
        raise SmokeFailure("unexpected_answer")


def validate_sse(parsed: dict, headers, provider: str, conversation_id: str) -> None:
    if headers.get("X-Gateway-Provider", "").lower() != provider:
        raise SmokeFailure("wrong_provider")
    if headers.get("X-Conversation-Id") != conversation_id:
        raise SmokeFailure("wrong_conversation")
    if _request_id(headers) is None:
        raise SmokeFailure("missing_request_id")
    if parsed["provider_error"] or not parsed["saw_done"]:
        raise SmokeFailure("sse_incomplete")
    if parsed["content"] != "OK":
        raise SmokeFailure("unexpected_answer")


def _call(base_url: str, api_key: str, model: str, provider: str,
          stream: bool, timeout: int, opener) -> dict:
    conversation_id = str(uuid.uuid4())
    body = json.dumps({
        "model": model, "stream": stream,
        "messages": [{"role": "user", "content": PROMPT}],
    }, separators=(",", ":")).encode("utf-8")
    headers = gateway_headers(
        api_key, "text/event-stream" if stream else "application/json"
    )
    headers["Content-Type"] = "application/json"
    headers["X-Conversation-Id"] = conversation_id
    req = urllib.request.Request(
        base_url.rstrip("/") + "/v1/chat/completions",
        data=body, headers=headers, method="POST",
    )
    start = time.monotonic()
    with opener.open(req, timeout=timeout) as response:
        request_id = _request_id(response.headers)
        if not 200 <= response.status < 300:
            raise SmokeFailure("invalid_response")
        if stream:
            parsed = consume_sse(bounded_sse_lines(response), start)
            validate_sse(parsed, response.headers, provider, conversation_id)
        else:
            parsed = _json_document(_read_limited(response))
            validate_json(parsed, response.headers, provider, conversation_id)
    # Keep only a canonical UUID and bounded timing; never store parsed content.
    return {
        "status": "passed", "request_id": request_id,
        "duration_ms": min(600_000, max(0, int((time.monotonic() - start) * 1000))),
    }


def _skipped(reason: str) -> dict:
    return {"status": "skipped", "reason": reason}


def parse_selected_providers(raw: str) -> tuple[str, ...]:
    # A fixed allowlist prevents accidental provider access. The owner must
    # explicitly name each deferred provider when resuming its investigation.
    selected = tuple(part.strip() for part in raw.split(","))
    if (not selected or any(part not in PROVIDERS for part in selected)
            or len(set(selected)) != len(selected)):
        raise SmokeFailure("invalid_selection")
    return selected


def run_matrix(
    models: dict[str, str], sender,
    selected_providers: tuple[str, ...] = FOCUS_PROVIDERS,
) -> list[dict]:
    selected = set(parse_selected_providers(",".join(selected_providers)))
    results = []
    for provider in PROVIDERS:
        item = {
            "provider": provider, "json": _skipped("model_not_advertised"),
            "sse": _skipped("json_not_passed"),
            "repeat": _skipped("separate_acceptance"),
            "restart": _skipped("separate_acceptance"),
        }
        if provider not in selected:
            item["json"] = _skipped("owner_deferred")
            item["sse"] = _skipped("owner_deferred")
            results.append(item)
            continue
        model = models.get(provider)
        if model:
            try:
                item["json"] = sender(provider, model, False)
            except Exception as error:
                item["json"] = {"status": "failed", "category": _safe_error(error)}
            if item["json"]["status"] == "passed":
                try:
                    item["sse"] = sender(provider, model, True)
                except Exception as error:
                    item["sse"] = {"status": "failed", "category": _safe_error(error)}
        results.append(item)
    return results


def _get_json(base_url: str, api_key: str, path: str, timeout: int, opener) -> dict:
    req = urllib.request.Request(
        base_url.rstrip("/") + path,
        headers=gateway_headers(api_key, "application/json"),
        method="GET",
    )
    with opener.open(req, timeout=timeout) as response:
        if response.status != 200:
            raise SmokeFailure("invalid_response")
        return _json_document(_read_limited(response))


def _report(phase: str, identity: dict | None = None,
            results: list[dict] | None = None, reason: str | None = None) -> dict:
    report = {
        "schema_version": 1,
        "phase": phase,
        "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "identity": identity or {"health": "unverified"},
        "providers": results if results is not None else [
            {"provider": provider, "json": _skipped(reason or "not_opted_in"),
             "sse": _skipped(reason or "not_opted_in"),
             "repeat": _skipped("separate_acceptance"),
             "restart": _skipped("separate_acceptance")}
            for provider in PROVIDERS
        ],
        "session_import_interference": "unknown",
    }
    return report


def execute(
    base_url: str, api_key: str, timeout: int, opener=None,
    selected_providers: tuple[str, ...] = FOCUS_PROVIDERS,
) -> dict:
    # Validate before any network call, even the local read-only preflight.
    parse_selected_providers(",".join(selected_providers))
    validate_canary_target(base_url, api_key, local_no_key=True)
    if timeout < 1 or timeout > 600:
        raise SmokeFailure("invalid_response")
    opener = opener or urllib.request.build_opener(_NoRedirect())
    health = _get_json(base_url, api_key, "/health", timeout, opener)
    identity = _identity(health)
    if identity["health"] != "ok":
        return _report("preflight_failed", identity, reason="gateway_not_ready")
    models = model_inventory(_get_json(base_url, api_key, "/v1/models", timeout, opener))
    results = run_matrix(
        models, lambda provider, model, stream: _call(
            base_url, api_key, model, provider, stream, timeout, opener
        ), selected_providers
    )
    return _report("completed", identity, results)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default=os.environ.get(
        "CHAT_GATEWAY_URL", "http://127.0.0.1:8080"))
    parser.add_argument("--timeout", type=int, default=150)
    parser.add_argument("--execute", action="store_true",
                        help="Explicitly authorize synthetic live requests")
    parser.add_argument("--providers", default="chatgpt,metaai",
                        help="Comma-separated explicit provider allowlist (default: chatgpt,metaai)")
    args = parser.parse_args()
    try:
        selected = parse_selected_providers(args.providers)
    except SmokeFailure:
        print(json.dumps(_report("preflight_failed", reason="invalid_selection"), sort_keys=True))
        return 1
    if not args.execute:
        print(json.dumps(_report("not_executed"), sort_keys=True))
        return 0
    try:
        report = execute(args.base_url, os.environ.get("CHAT_GATEWAY_API_KEY", ""),
                         args.timeout, selected_providers=selected)
    except Exception as error:
        # Never reflect upstream error text, URLs, credentials or prompt content.
        report = _report("preflight_failed", reason=_safe_error(error))
    print(json.dumps(report, sort_keys=True))
    return 0 if report["phase"] == "completed" and all(
        item["json"]["status"] == "passed"
        and item["sse"]["status"] == "passed"
        for item in report["providers"] if item["provider"] in selected
    ) else 1


if __name__ == "__main__":
    raise SystemExit(main())
