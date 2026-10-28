#!/usr/bin/env python3
"""Reproducible end-to-end streaming benchmark for Chat-Gateway.

Measures client-observed TTFT, total response latency, inter-provider switch
TTFT, output size, and optional Linux gateway RSS (when --server-pid is given).
Uses only Python's standard library and never logs prompts, tokens, or headers.
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import time
import urllib.error
import urllib.request
from typing import Any


def rss_kib(pid: int | None) -> int | None:
    if pid is None:
        return None
    try:
        with open(f"/proc/{pid}/status", encoding="utf-8") as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (OSError, ValueError):
        return None
    return None


def first_content(payload: dict[str, Any]) -> str:
    choices = payload.get("choices") or []
    if not choices:
        return ""
    choice = choices[0] or {}
    delta = choice.get("delta") or {}
    content = delta.get("content")
    if content is None:
        message = choice.get("message") or {}
        content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            item.get("text", "") for item in content if isinstance(item, dict)
        )
    return ""


def run_once(args: argparse.Namespace, model: str, previous_provider: str | None) -> dict[str, Any]:
    url = args.base_url.rstrip("/") + "/v1/chat/completions"
    body = json.dumps({
        "model": model,
        "stream": True,
        "messages": [{"role": "user", "content": args.prompt}],
    }).encode("utf-8")
    headers = {"Content-Type": "application/json", "Accept": "text/event-stream"}
    if args.api_key:
        headers["Authorization"] = "Bearer " + args.api_key
    request = urllib.request.Request(url, data=body, headers=headers, method="POST")
    started = time.perf_counter()
    first_token: float | None = None
    output_chars = 0
    status = 0
    provider = ""
    rss_before = rss_kib(args.server_pid)
    try:
        with urllib.request.urlopen(request, timeout=args.timeout) as response:
            status = response.status
            provider = response.headers.get("X-Gateway-Provider", "")
            for raw_line in response:
                if time.perf_counter() - started > args.timeout:
                    raise TimeoutError("stream exceeded benchmark timeout")
                line = raw_line.decode("utf-8", errors="replace").strip()
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if not data or data == "[DONE]":
                    continue
                try:
                    payload = json.loads(data)
                except json.JSONDecodeError:
                    continue
                content = first_content(payload)
                if content:
                    if first_token is None:
                        first_token = time.perf_counter()
                    output_chars += len(content)
    except urllib.error.HTTPError as error:
        detail = error.read(1024).decode("utf-8", errors="replace")
        raise RuntimeError(f"HTTP {error.code} for model {model!r}: {detail}") from error
    ended = time.perf_counter()
    rss_after = rss_kib(args.server_pid)
    return {
        "model": model,
        "provider": provider or None,
        "http_status": status,
        "ttft_ms": round((first_token - started) * 1000, 2) if first_token else None,
        "total_ms": round((ended - started) * 1000, 2),
        "post_ttft_ms": round((ended - first_token) * 1000, 2) if first_token else None,
        "output_chars": output_chars,
        "provider_switch": previous_provider is not None and previous_provider != (provider or model),
        "gateway_rss_before_kib": rss_before,
        "gateway_rss_after_kib": rss_after,
    }


def percentile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    return round(ordered[min(len(ordered) - 1, int((len(ordered) - 1) * fraction))], 2)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8080")
    parser.add_argument("--models", required=True,
                        help="comma-separated model aliases; use aliases configured by the gateway")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--prompt", default="Reply with exactly: benchmark-ok")
    parser.add_argument("--api-key", default=os.environ.get("CHAT_GATEWAY_API_KEY"))
    parser.add_argument("--server-pid", type=int,
                        help="optional Linux PID of gateway process to sample VmRSS")
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args()
    models = [item.strip() for item in args.models.split(",") if item.strip()]
    if not models:
        parser.error("--models must contain at least one model alias")
    if args.rounds < 1:
        parser.error("--rounds must be >= 1")
    results = []
    previous_provider = None
    for round_index in range(args.rounds):
        for model in models:
            try:
                result = run_once(args, model, previous_provider)
            except Exception as error:  # emit a useful failure record, then fail the run
                print(json.dumps({"round": round_index + 1, "model": model,
                                  "error": str(error)}, sort_keys=True), flush=True)
                return 1
            result["round"] = round_index + 1
            results.append(result)
            print(json.dumps(result, sort_keys=True), flush=True)
            previous_provider = result["provider"] or model
    ttft = [row["ttft_ms"] for row in results if row["ttft_ms"] is not None]
    total = [row["total_ms"] for row in results]
    switches = [row["ttft_ms"] for row in results
                if row["provider_switch"] and row["ttft_ms"] is not None]
    rss = [row["gateway_rss_after_kib"] for row in results
           if row["gateway_rss_after_kib"] is not None]
    summary = {
        "runs": len(results),
        "models": models,
        "ttft_ms": {"median": round(statistics.median(ttft), 2) if ttft else None,
                    "p95": percentile(ttft, 0.95)},
        "total_ms": {"median": round(statistics.median(total), 2) if total else None,
                     "p95": percentile(total, 0.95)},
        "provider_switch_ttft_ms": {
            "median": round(statistics.median(switches), 2) if switches else None,
            "samples": len(switches),
        },
        "gateway_rss_kib": {"min": min(rss), "max": max(rss)} if rss else None,
        "note": "Client-observed timings; compare on the same host, prompt, model order, and session state.",
    }
    print(json.dumps({"summary": summary}, sort_keys=True), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
