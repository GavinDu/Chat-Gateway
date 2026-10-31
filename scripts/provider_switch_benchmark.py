#!/usr/bin/env python3
"""Measure first-request latency after provider switches vs same-provider follow-ups.

Requires intentionally enabled, authenticated provider sessions. Reports only
aggregate timing and failure categories; prompts and responses are never printed.
"""
from __future__ import annotations

import argparse
import json
import math
import os
import time
import urllib.error
import urllib.request


def percentile(values: list[int], p: float) -> int | None:
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, min(len(ordered) - 1, math.ceil(p * len(ordered) / 100) - 1))]


def consume_sse(response, started: float) -> tuple[int | None, bool, bool]:
    ttft = None
    done = False
    provider_error = False
    for raw in response:
        line = raw.decode("utf-8", "replace") if isinstance(raw, bytes) else raw
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            done = True
            break
        if not data:
            continue
        try:
            event = json.loads(data)
        except json.JSONDecodeError:
            provider_error = True
            break
        if event.get("error") is not None:
            provider_error = True
            break
        choices = event.get("choices") or []
        if choices:
            delta = choices[0].get("delta") or {}
            content = delta.get("content")
            if isinstance(content, str) and content and ttft is None:
                ttft = int((time.monotonic() - started) * 1000)
    return ttft, done, provider_error


def parse_step(value: str) -> tuple[str, str]:
    provider, separator, model = value.partition("=")
    if not separator or not provider or not model:
        raise argparse.ArgumentTypeError("each --step must be provider=model")
    return provider.lower(), model


def request_turn(base_url: str, api_key: str, provider: str, model: str,
                 conversation_id: str | None, sequence_number: int,
                 timeout: int) -> dict:
    payload = json.dumps({
        "model": model,
        "stream": True,
        "messages": [{"role": "user", "content":
            f"Reply with one short status sentence. Provider switch sample {sequence_number}."}],
    }).encode("utf-8")
    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
        "Accept": "text/event-stream",
    }
    if conversation_id:
        headers["X-Conversation-Id"] = conversation_id
    request = urllib.request.Request(
        base_url.rstrip("/") + "/v1/chat/completions",
        data=payload,
        headers=headers,
        method="POST",
    )
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            actual_provider = response.headers.get("X-Gateway-Provider", "").lower()
            returned_conversation = response.headers.get("X-Conversation-Id")
            ttft, done, provider_error = consume_sse(response, started)
            status = response.status
    except urllib.error.HTTPError as error:
        return {"ok": False, "failure": f"http_{error.code}"}
    except (urllib.error.URLError, TimeoutError, OSError):
        return {"ok": False, "failure": "gateway_unreachable_or_timeout"}
    total = int((time.monotonic() - started) * 1000)
    if status < 200 or status >= 300:
        return {"ok": False, "failure": f"http_{status}"}
    if actual_provider != provider:
        return {"ok": False, "failure": "provider_identity_mismatch"}
    if conversation_id and returned_conversation != conversation_id:
        return {"ok": False, "failure": "conversation_id_changed"}
    if not returned_conversation:
        return {"ok": False, "failure": "conversation_id_missing"}
    if provider_error:
        return {"ok": False, "failure": "provider_stream_error"}
    if ttft is None:
        return {"ok": False, "failure": "missing_first_token"}
    if not done:
        return {"ok": False, "failure": "missing_sse_done"}
    return {"ok": True, "ttft_ms": ttft, "total_ms": total,
            "conversation_id": returned_conversation}


def latency_category(previous_provider: str | None, provider: str) -> str:
    if previous_provider is None:
        return "initial_provider_request"
    if previous_provider != provider:
        return "after_provider_switch"
    return "same_provider_follow_up"


def summarize(values: list[int]) -> dict:
    return {"samples": len(values), "p50_ms": percentile(values, 50),
            "p95_ms": percentile(values, 95)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--step", action="append", type=parse_step, required=True,
                        help="ordered provider=model step; repeat to describe switches")
    parser.add_argument("--base-url", default=os.environ.get(
        "CHAT_GATEWAY_URL", "http://127.0.0.1:8080"))
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args()
    api_key = os.environ.get("CHAT_GATEWAY_API_KEY", "")
    if not api_key:
        parser.error("set CHAT_GATEWAY_API_KEY in the environment")
    if args.repeat < 1 or args.timeout < 1 or len(args.step) < 2:
        parser.error("--repeat and --timeout must be positive; at least two --step values are required")

    categories = {
        "initial_provider_request": [],
        "after_provider_switch": [],
        "same_provider_follow_up": [],
    }
    failures: dict[str, int] = {}
    for repetition in range(args.repeat):
        conversation_ids: dict[str, str] = {}
        previous_provider = None
        for index, (provider, model) in enumerate(args.step):
            existing_conversation = conversation_ids.get(provider)
            result = request_turn(
                args.base_url, api_key, provider, model, existing_conversation,
                repetition * len(args.step) + index, args.timeout,
            )
            if not result["ok"]:
                failure = result["failure"]
                failures[f"{provider}:{failure}"] = failures.get(f"{provider}:{failure}", 0) + 1
            else:
                conversation_ids[provider] = result["conversation_id"]
                category = latency_category(previous_provider, provider)
                categories[category].append(result["ttft_ms"])
                categories[category + "_total"] = categories.get(category + "_total", [])
                categories[category + "_total"].append(result["total_ms"])
            # Classification follows the requested sequence, not successful
            # completions. A failed step must not make the next provider look
            # like an initial request or a same-provider follow-up.
            previous_provider = provider

    report = {
        "sequence": [provider for provider, _ in args.step],
        "repetitions": args.repeat,
        "failures": failures,
        "latency": {
            category: summarize(values)
            for category, values in categories.items()
            if not category.endswith("_total")
        },
        "total_latency": {
            category.removesuffix("_total"): summarize(values)
            for category, values in categories.items()
            if category.endswith("_total")
        },
    }
    print(json.dumps(report, sort_keys=True))
    return 0 if not failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
