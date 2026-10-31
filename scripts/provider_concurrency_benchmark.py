#!/usr/bin/env python3
"""Measure live provider TTFT/total latency at concurrency 1, 2 and 4.

Output is aggregate-only: no prompts, response text, cookies or credentials.
Requires an explicitly enabled, authenticated provider session.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import json
import math
import os
import statistics
import subprocess
import time
import urllib.error
import urllib.request


def percentile(values: list[int], p: float) -> int | None:
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil((p / 100) * len(ordered)) - 1))
    return ordered[index]


def parse_sse(lines, request_started: float) -> tuple[int | None, bool, bool]:
    first_token_ms = None
    saw_done = False
    provider_error = False
    for raw in lines:
        line = raw.decode("utf-8", "replace") if isinstance(raw, bytes) else raw
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if payload == "[DONE]":
            saw_done = True
            break
        if not payload:
            continue
        try:
            event = json.loads(payload)
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
            if isinstance(content, str) and content and first_token_ms is None:
                first_token_ms = int((time.monotonic() - request_started) * 1000)
    return first_token_ms, saw_done, provider_error


def process_rss_kb(pid: int | None) -> int | None:
    if pid is None:
        return None
    try:
        output = subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(pid)],
            text=True, stderr=subprocess.DEVNULL, timeout=2,
        ).strip()
        return int(output) if output else None
    except (OSError, subprocess.SubprocessError, ValueError):
        return None


def one_request(base_url: str, api_key: str, provider: str, model: str,
                timeout: int, sample: int) -> dict:
    payload = json.dumps({
        "model": model,
        "stream": True,
        "messages": [{"role": "user", "content":
            f"Reply with a short status sentence. Benchmark sample {sample}."}],
    }).encode("utf-8")
    request = urllib.request.Request(
        base_url.rstrip("/") + "/v1/chat/completions",
        data=payload,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "Accept": "text/event-stream",
        },
        method="POST",
    )
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            actual_provider = response.headers.get("X-Gateway-Provider", "").lower()
            ttft_ms, saw_done, provider_error = parse_sse(response, started)
            status = response.status
    except urllib.error.HTTPError as error:
        return {"ok": False, "failure": f"http_{error.code}"}
    except (urllib.error.URLError, TimeoutError, OSError):
        return {"ok": False, "failure": "gateway_unreachable_or_timeout"}
    total_ms = int((time.monotonic() - started) * 1000)
    if status < 200 or status >= 300:
        return {"ok": False, "failure": f"http_{status}"}
    if actual_provider != provider:
        return {"ok": False, "failure": "provider_identity_mismatch"}
    if provider_error:
        return {"ok": False, "failure": "provider_stream_error"}
    if ttft_ms is None:
        return {"ok": False, "failure": "missing_first_token"}
    if not saw_done:
        return {"ok": False, "failure": "missing_sse_done"}
    return {"ok": True, "ttft_ms": ttft_ms, "total_ms": total_ms}


def summarize(base_url: str, api_key: str, provider: str, model: str,
              concurrency: int, samples: int, timeout: int, pid: int | None) -> dict:
    rss_before = process_rss_kb(pid)
    started = time.monotonic()
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
        futures = [
            executor.submit(one_request, base_url, api_key, provider, model, timeout,
                            concurrency * 100000 + index)
            for index in range(samples)
        ]
        for future in concurrent.futures.as_completed(futures):
            try:
                results.append(future.result())
            except Exception:
                results.append({"ok": False, "failure": "benchmark_worker_error"})
    elapsed_ms = int((time.monotonic() - started) * 1000)
    successes = [item for item in results if item["ok"]]
    failures: dict[str, int] = {}
    for item in results:
        if not item["ok"]:
            failures[item["failure"]] = failures.get(item["failure"], 0) + 1
    ttft = [item["ttft_ms"] for item in successes]
    total = [item["total_ms"] for item in successes]
    rss_after = process_rss_kb(pid)
    return {
        "concurrency": concurrency,
        "samples": samples,
        "successes": len(successes),
        "failures": failures,
        "ttft_ms": {"p50": percentile(ttft, 50), "p95": percentile(ttft, 95)},
        "total_ms": {"p50": percentile(total, 50), "p95": percentile(total, 95)},
        "elapsed_ms": elapsed_ms,
        "gateway_rss_kb_before": rss_before,
        "gateway_rss_kb_after": rss_after,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", required=True,
                        choices=("chatgpt", "claude", "gemini", "grok", "qwen", "metaai", "mistral", "perplexity"))
    parser.add_argument("--model", required=True)
    parser.add_argument("--base-url", default=os.environ.get("CHAT_GATEWAY_URL", "http://127.0.0.1:8080"))
    parser.add_argument("--samples", type=int, default=8)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--pid", type=int, help="Gateway PID for before/after RSS samples")
    parser.add_argument("--concurrency", type=int, nargs="+", default=[1, 2, 4])
    args = parser.parse_args()
    api_key = os.environ.get("CHAT_GATEWAY_API_KEY", "")
    if not api_key:
        parser.error("set CHAT_GATEWAY_API_KEY in the environment")
    if args.samples < 1 or args.timeout < 1:
        parser.error("--samples and --timeout must be positive")
    if not args.concurrency or any(value < 1 or value > 32 for value in args.concurrency):
        parser.error("--concurrency values must be between 1 and 32")
    report = {
        "provider": args.provider,
        "model": args.model,
        "sample_count_per_level": args.samples,
        "results": [
            summarize(args.base_url, api_key, args.provider, args.model,
                      level, args.samples, args.timeout, args.pid)
            for level in args.concurrency
        ],
    }
    print(json.dumps(report, sort_keys=True))
    return 0 if all(row["successes"] == row["samples"] for row in report["results"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
