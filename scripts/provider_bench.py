#!/usr/bin/env python3
"""Reproducible Chat Gateway benchmark: fake mode is deterministic plumbing smoke;
live mode measures an OpenAI-compatible endpoint (network/provider latency included)."""
import argparse
import json
import os
import statistics
import time
import urllib.request
from datetime import datetime, timezone


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
    line = sse_line.strip()
    if not line.startswith("data:"):
        return False
    data = line[5:].strip()
    if not data or data == "[DONE]":
        return False
    try:
        event = json.loads(data)
    except json.JSONDecodeError:
        return False
    content = (event.get("choices") or [{}])[0].get("delta", {}).get("content")
    return isinstance(content, str) and bool(content)


def live_trial(args):
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
    first_content_delta = None
    with urllib.request.urlopen(req, timeout=args.timeout) as response:
        for line in response:
            if has_content_delta(line):
                first_content_delta = time.perf_counter()
                break
    if first_content_delta is None:
        raise RuntimeError("endpoint completed without a streamed content delta")
    return (first_content_delta - started) * 1000, None


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
