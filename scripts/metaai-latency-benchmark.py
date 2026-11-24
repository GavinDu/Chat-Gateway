#!/usr/bin/env python3
"""Measure real Meta AI SSE completion, without retaining prompts/responses.

Single-flight canaries use fixed-length random markers to verify content and
avoid confusing repeated answers with stale DOM. This is an explicit live test,
not a recurring health probe. API keys are read only from the environment.
"""
import argparse
import json
import math
import os
from pathlib import Path
import secrets
import statistics
import time
import urllib.error
import urllib.parse
import urllib.request
import re
import uuid


class NoRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class SseMeasurement:
    def __init__(self, expected):
        self.expected = expected
        self.first = None
        self.last = None
        self.done = None
        self.content = ""
        self.finish_reason = None
        self.error = False
        self.delta_count = 0
        self.max_delta_gap = 0

    def observe(self, line, elapsed):
        if not line.startswith(b"data:"):
            return
        data = line[5:].strip()
        if self.done is not None:
            self.error = True
            return
        if data == b"[DONE]":
            self.done = elapsed
            return
        try:
            event = json.loads(data)
        except (ValueError, UnicodeDecodeError):
            self.error = True
            return
        if not isinstance(event, dict):
            self.error = True
            return
        if event.get("error"):
            self.error = True
        choices = event.get("choices", [])
        if not isinstance(choices, list):
            self.error = True
            return
        for choice in choices:
            if not isinstance(choice, dict) or not isinstance(choice.get("delta", {}), dict):
                self.error = True
                continue
            if choice.get("finish_reason") is not None:
                self.finish_reason = choice["finish_reason"]
            text = choice.get("delta", {}).get("content")
            if isinstance(text, str) and text:
                if self.first is None:
                    self.first = elapsed
                if self.last is not None:
                    self.max_delta_gap = max(self.max_delta_gap, elapsed-self.last)
                self.last = elapsed
                self.delta_count += 1
                # Retain only a small synthetic canary answer in memory.
                if len(self.content)+len(text) > 4096:
                    self.error = True
                self.content = (self.content + text)[:4096]

    def result(self, elapsed):
        success = (not self.error and self.done is not None
                   and self.finish_reason == "stop"
                   and self.content.strip() == self.expected)
        return {"success": success, "first_content_ms": self.first,
                "last_content_ms": self.last, "done_ms": self.done,
                "total_ms": elapsed,
                "tail_ms": None if self.last is None or self.done is None else round(max(0, self.done-self.last), 3),
                "max_delta_gap_ms": round(self.max_delta_gap, 3),
                "delta_count": self.delta_count,
                "output_chars": len(self.content), "finish_reason": self.finish_reason,
                "terminal_done_observed": self.done is not None}


def percentile(values, q):
    values = sorted(values)
    return values[max(0, math.ceil(q*len(values))-1)] if values else None


def summarize(rows):
    successful = [r for r in rows if r.get("success")]
    output = {"samples": len(rows), "successful": len(successful),
              "failures": len(rows)-len(successful),
              "p95_minimum_sample_count_met": len(successful) >= 20,
              "caution": "Small-sample p95 is not an SLO; compare like-for-like cohorts and retain failures."}
    for key in ["first_content_ms", "total_ms", "tail_ms"]:
        values = [r[key] for r in successful if r.get(key) is not None]
        output[key] = {"n": len(values), "p50": statistics.median(values) if values else None,
                       "p95": percentile(values, .95), "max": max(values) if values else None}
    return output


def run_one(base, timeout, api_key):
    marker = "CG_LATENCY_" + secrets.token_hex(6)
    body = {"model": "metaai/meta-ai", "stream": True,
            "messages": [{"role": "user", "content": "Reply exactly " + marker}]}
    headers = {"Content-Type": "application/json", "X-Chat-Gateway-Request-Kind": "canary"}
    if api_key:
        headers["Authorization"] = "Bearer " + api_key
    request = urllib.request.Request(base.rstrip("/")+"/v1/chat/completions",
                                     data=json.dumps(body).encode(), headers=headers)
    measurement = SseMeasurement(marker)
    started = time.monotonic()
    record = {"kind": "canary", "transport": "sse", "status": None,
              "request_id": None, "failure_category": None}
    try:
        with urllib.request.build_opener(NoRedirects()).open(request, timeout=timeout) as response:
            record["status"] = response.status
            record["headers_ms"] = round((time.monotonic()-started)*1000, 3)
            record["request_id"] = response.headers.get("x-request-id")
            for line in response:
                if time.monotonic()-started > timeout:
                    raise TimeoutError()
                if len(line) > 1048576:
                    raise ValueError("oversized_sse_line")
                measurement.observe(line, round((time.monotonic()-started)*1000, 3))
    except urllib.error.HTTPError as error:
        record["status"] = error.code
        record["request_id"] = error.headers.get("x-request-id")
        record["failure_category"] = "http_error"
        error.close()
    except (TimeoutError, OSError, ValueError):
        record["failure_category"] = "transport_or_stream_error"
    if record["request_id"] is not None:
        try:
            record["request_id"] = str(uuid.UUID(record["request_id"]))
        except (ValueError, AttributeError):
            record["request_id"] = None
    record.update(measurement.result(round((time.monotonic()-started)*1000, 3)))
    record["success"] = record["success"] and record["status"] == 200
    if not record["success"] and record["failure_category"] is None:
        record["failure_category"] = "invalid_or_incomplete_completion"
    return record


def read_build(base, timeout, api_key):
    request = urllib.request.Request(base.rstrip("/")+"/health")
    if api_key:
        request.add_header("Authorization", "Bearer "+api_key)
    with urllib.request.build_opener(NoRedirects()).open(request, timeout=timeout) as response:
        raw = response.read(65537)
        if len(raw) > 65536:
            raise ValueError("oversized_health")
        payload = json.loads(raw)
    build = payload.get("build", {}) if isinstance(payload, dict) else {}
    if not isinstance(build, dict) or not isinstance(build.get("source_sha"), str) or not re.fullmatch(r"[0-9a-f]{40}", build["source_sha"]):
        raise ValueError("gateway_revision_unattributed")
    return {key: build.get(key) for key in ["version", "source_sha", "build_timestamp_utc"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8080")
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument("--interval", type=float, default=5)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    url = urllib.parse.urlsplit(args.base_url)
    if url.scheme not in ["http", "https"] or not url.hostname or url.username or url.password or url.query or url.fragment or url.path not in ["", "/"]:
        parser.error("Use an HTTP(S) origin without credentials, query or fragment.")
    if (not 1 <= args.samples <= 100 or not 0 <= args.warmup <= 10
            or not math.isfinite(args.timeout) or not 0 < args.timeout <= 600
            or not math.isfinite(args.interval) or not 0 <= args.interval <= 3600):
        parser.error("Invalid sample count, warmup, timeout or interval.")
    report = {"schema_version": 1, "model": "metaai/meta-ai", "stream": True,
              "source": "synthetic_single_flight_canary", "warmup_requested": args.warmup, "warmup_excluded": 0,
              "requested_samples": args.samples,
              "timing_clock": "monotonic", "records": [], "warmup_failures": 0}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # Do not silently append to another run or leave output readable to others.
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(fd)
    api_key = os.environ.get("CHAT_GATEWAY_API_KEY")
    report["workload"] = "synthetic_short_echo"
    report["client_settings"] = {"timeout_seconds": args.timeout, "interval_seconds": args.interval}
    try:
        report["gateway_build_start"] = read_build(args.base_url, min(args.timeout, 10), api_key)
    except (OSError, ValueError):
        report["setup_failure"] = "gateway_build_unavailable_or_unattributed"
        report["summary"] = summarize([])
        report["summary"].update(cohort_complete=False, release_unchanged=False)
        args.output.write_text(json.dumps(report, indent=2)+"\n")
        print(json.dumps(report["summary"]), flush=True)
        return 1
    for index in range(args.warmup+args.samples):
        row = run_one(args.base_url, args.timeout, api_key)
        if index < args.warmup:
            report["warmup_excluded"] += 1
            report["warmup_failures"] += int(not row["success"])
        else:
            report["records"].append(row)
        report["summary"] = summarize(report["records"])
        report["summary"]["cohort_complete"] = len(report["records"]) == args.samples
        args.output.write_text(json.dumps(report, indent=2)+"\n")
        print(json.dumps({"sample": index-args.warmup+1, "warmup": index < args.warmup,
                          **row}), flush=True)
        # A failed provider attempt is evidence, not an invitation to hammer cooldown.
        if not row["success"]:
            break
        if index+1 < args.warmup+args.samples:
            time.sleep(args.interval)
    try:
        report["gateway_build_end"] = read_build(args.base_url, min(args.timeout, 10), api_key)
        report["release_unchanged"] = report["gateway_build_start"] == report["gateway_build_end"]
    except (OSError, ValueError):
        report["release_unchanged"] = False
    report["summary"]["release_unchanged"] = report["release_unchanged"]
    args.output.write_text(json.dumps(report, indent=2)+"\n")
    print(json.dumps(report["summary"]), flush=True)
    return 0 if report["release_unchanged"] and len(report["records"]) == args.samples and all(r["success"] for r in report["records"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
