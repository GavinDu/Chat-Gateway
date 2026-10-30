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


def live_trial(args):
    payload = json.dumps({
        "model": args.model,
        "stream": True,
        "messages": [{"role": "user", "content": args.prompt}],
        "max_tokens": args.max_tokens,
    }).encode()
    req = urllib.request.Request(
        args.endpoint.rstrip("/") + "/chat/completions",
        data=payload,
        headers={"Content-Type": "application/json",
                 "Authorization": "Bearer " + args.api_key},
        method="POST",
    )
    started = time.perf_counter()
    with urllib.request.urlopen(req, timeout=args.timeout) as response:
        first_byte = None
        while response.read(1):
            if first_byte is None:
                first_byte = time.perf_counter()
    if first_byte is None:
        raise RuntimeError("endpoint returned an empty response")
    return (first_byte - started) * 1000, None


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
    p.add_argument("--max-tokens", type=int, default=32)
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
                  else "TTFT is time to first response byte; endpoint/network included"),
    }
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
