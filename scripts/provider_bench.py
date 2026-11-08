#!/usr/bin/env python3
"""Reproducible Chat Gateway benchmark: fake mode is deterministic plumbing smoke;
live mode measures an OpenAI-compatible endpoint (network/provider latency included)."""
import argparse
import json
import os
import statistics
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone

MAX_SSE_LINE_BYTES = 65_536
MAX_SSE_STREAM_BYTES = 2_097_152


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        return None


def validate_benchmark_target(endpoint):
    """Allow private/local gateway endpoints without leaking API auth on redirects."""
    try:
        target = urllib.parse.urlsplit(endpoint)
        port = target.port
    except ValueError:
        raise RuntimeError("invalid_benchmark_endpoint") from None
    local = target.hostname in {"127.0.0.1", "localhost", "::1"}
    if (
        target.scheme not in ("http", "https")
        or not target.netloc
        or target.username is not None
        or target.password is not None
        or target.path.rstrip("/") != "/v1"
        or target.query
        or target.fragment
        or port == 0
        or (target.scheme == "http" and not local)
    ):
        raise RuntimeError("invalid_benchmark_endpoint")


def consume_live_benchmark_sse(response, started):
    """Return TTFT only for a complete successful stream; never store content."""
    first_content_delta = None
    saw_done = False
    saw_terminal_stop = False
    consumed_bytes = 0
    while True:
        line = response.readline(MAX_SSE_LINE_BYTES + 1)
        if not line:
            break
        if len(line) > MAX_SSE_LINE_BYTES:
            raise RuntimeError("benchmark_sse_line_too_large")
        consumed_bytes += len(line)
        if consumed_bytes > MAX_SSE_STREAM_BYTES:
            raise RuntimeError("benchmark_sse_stream_too_large")
        if not line.startswith(b"data:"):
            continue
        data = line[5:].strip()
        if data == b"[DONE]":
            saw_done = True
            break
        if not data:
            continue
        try:
            event = json.loads(data)
        except (ValueError, UnicodeDecodeError):
            raise RuntimeError("benchmark_invalid_stream_event") from None
        if not isinstance(event, dict):
            raise RuntimeError("benchmark_invalid_stream_event")
        if event.get("error") is not None:
            raise RuntimeError("benchmark_provider_stream_failed")
        choices = event.get("choices")
        if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
            raise RuntimeError("benchmark_invalid_stream_event")
        finish_reason = choices[0].get("finish_reason")
        if finish_reason is not None:
            if finish_reason != "stop":
                raise RuntimeError("benchmark_provider_stream_failed")
            saw_terminal_stop = True
        if first_content_delta is None and has_content_delta(line):
            first_content_delta = time.perf_counter()

    if first_content_delta is None:
        raise RuntimeError("endpoint completed without a streamed content delta")
    if not saw_done or not saw_terminal_stop:
        raise RuntimeError("benchmark_incomplete_stream")
    return (first_content_delta - started) * 1000


def rss_bytes():
    try:
        with open("/proc/self/statm", encoding="ascii") as f:
            pages = int(f.read().split()[1])
        return pages * os.sysconf("SC_PAGE_SIZE")
    except (OSError, ValueError, IndexError):
        return None


def fake_trial():
    start = time.perf_counter()
    time.sleep(0.005)
    ttft = (time.perf_counter() - start) * 1000
    # Explicitly synthetic provider switch cost; no browser/provider is involved.
    switch_start = time.perf_counter()
    time.sleep(0.002)
    switch_ms = (time.perf_counter() - switch_start) * 1000
    return ttft, switch_ms


def has_content_delta(sse_line):
    """Return true only for a non-empty OpenAI-compatible streamed content delta."""
    if isinstance(sse_line, bytes):
        sse_line = sse_line.decode("utf-8", errors="replace")
    if not isinstance(sse_line, str):
        return False
    line = sse_line.strip()
    if not line.startswith("data:"):
        return False
    data = line[5:].strip()
    if not data or data == "[DONE]":
        return False
    try:
        event = json.loads(data)
    except (json.JSONDecodeError, TypeError, ValueError):
        return False
    if not isinstance(event, dict):
        return False
    choices = event.get("choices")
    if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
        return False
    delta = choices[0].get("delta")
    if not isinstance(delta, dict):
        return False
    content = delta.get("content")
    return isinstance(content, str) and bool(content)


def live_trial(args):
    validate_benchmark_target(args.endpoint)
    # Keep this payload within the gateway's implemented OpenAI-compatible
    # request schema. max_tokens is not currently supported and is rejected.
    payload = json.dumps({
        "model": args.model,
        "stream": True,
        "messages": [{"role": "user", "content": args.prompt}],
    }).encode()
    req = urllib.request.Request(
        args.endpoint.rstrip("/") + "/chat/completions",
        data=payload,
        headers={"Content-Type": "application/json",
                 "Authorization": "Bearer " + args.api_key},
        method="POST",
    )
    started = time.perf_counter()
    try:
        # Never allow a redirect to replay an API key or prompt to another origin.
        opener = urllib.request.build_opener(_NoRedirect())
        with opener.open(req, timeout=args.timeout) as response:
            if response.status != 200:
                raise RuntimeError("benchmark_http_status_" + str(response.status))
            ttft = consume_live_benchmark_sse(response, started)
    except urllib.error.HTTPError as error:
        raise RuntimeError("benchmark_http_status_" + str(error.code)) from None
    except (urllib.error.URLError, OSError, TimeoutError):
        raise RuntimeError("benchmark_gateway_unreachable") from None
    return ttft, None


def summary(values):
    return {"samples": len(values), "median_ms": round(statistics.median(values), 3),
            "p95_ms": round(sorted(values)[min(len(values)-1, int(len(values)*0.95))], 3)}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--mode", choices=("fake", "live"), default="fake")
    p.add_argument("--iterations", type=int, default=20)
    p.add_argument("--endpoint", default=os.getenv("CHAT_GATEWAY_BENCH_ENDPOINT", ""))
    p.add_argument("--api-key", default=os.getenv("CHAT_GATEWAY_BENCH_API_KEY", ""))
    p.add_argument("--model", default=os.getenv("CHAT_GATEWAY_BENCH_MODEL", ""))
    p.add_argument("--prompt", default="Reply with one short sentence.")
    p.add_argument("--timeout", type=float, default=60)
    args = p.parse_args()
    if args.iterations < 1:
        p.error("--iterations must be positive")
    if args.mode == "live" and not all((args.endpoint, args.api_key, args.model)):
        p.error("live mode requires endpoint, API key and model via flags or CHAT_GATEWAY_BENCH_*")
    ttft, switches = [], []
    rss_before = rss_bytes()
    for _ in range(args.iterations):
        first, switch = fake_trial() if args.mode == "fake" else live_trial(args)
        ttft.append(first)
        if switch is not None:
            switches.append(switch)
    rss_after = rss_bytes()
    result = {
        "schema_version": 1,
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "mode": args.mode,
        "iterations": args.iterations,
        "environment": {"python": os.sys.version.split()[0], "platform": os.name,
                        "rss_before_bytes": rss_before, "rss_after_bytes": rss_after},
        "metrics": {"ttft": summary(ttft),
                    "switch_latency": summary(switches) if switches else None},
        "notes": ("synthetic timings; not provider/browser performance" if args.mode == "fake"
                  else "TTFT is time to first non-empty streamed content delta; endpoint/network included"),
    }
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
