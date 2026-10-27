#!/usr/bin/env python3
"""Reproducible Chat Gateway TTFT/switch-latency harness (stdlib only).

Fake mode measures client/harness overhead against a deterministic local SSE
backend. Live mode measures the configured gateway endpoint. Results are JSON
so they can be archived and compared without third-party Python packages.
"""
from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class FakeBackend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            time.sleep(0.015)
            self.wfile.write(b'data: {"choices":[{"delta":{"content":"hello"}}]}\\n\\n')
            self.wfile.flush()
            time.sleep(0.015)
            self.wfile.write(b'data: {"choices":[{"delta":{"content":" world"}}]}\\n\\n')
            self.wfile.flush()
            self.wfile.write(b"data: [DONE]\\n\\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, _format: str, *_args: Any) -> None:
        return


def percentile(values: list[float], pct: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    return round(ordered[min(len(ordered) - 1, int((len(ordered) - 1) * pct))], 3)


def process_rss_mb(pid: int | None) -> float | None:
    if pid is None:
        return None
    status = f"/proc/{pid}/status"
    try:
        with open(status, encoding="utf-8") as stream:
            for line in stream:
                if line.startswith("VmRSS:"):
                    return round(int(line.split()[1]) / 1024, 2)
    except OSError:
        pass
    try:
        output = subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(pid)], text=True, timeout=2
        ).strip()
        return round(int(output) / 1024, 2) if output else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def one_request(url: str, model: str, api_key: str | None, timeout: float) -> dict[str, Any]:
    body = json.dumps({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with a short greeting."}],
        "stream": True,
    }).encode()
    headers = {"Content-Type": "application/json", "Accept": "text/event-stream"}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    request = urllib.request.Request(url, data=body, headers=headers, method="POST")
    started = time.perf_counter()
    first_token_ms: float | None = None
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            if response.status < 200 or response.status >= 300:
                raise RuntimeError(f"HTTP {response.status}")
            for raw_line in response:
                if not raw_line.startswith(b"data:"):
                    continue
                payload = raw_line[5:].strip()
                if payload and payload != b"[DONE]":
                    first_token_ms = (time.perf_counter() - started) * 1000
                    # Consume the stream to measure full request latency.
                    break
            for _ in response:
                pass
    except urllib.error.HTTPError as error:
        detail = error.read(512).decode("utf-8", "replace")
        raise RuntimeError(f"HTTP {error.code}: {detail}") from error
    elapsed_ms = (time.perf_counter() - started) * 1000
    if first_token_ms is None:
        raise RuntimeError("stream ended without a data event")
    return {
        "ttft_ms": round(first_token_ms, 3),
        "total_ms": round(elapsed_ms, 3),
        "model": model,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("fake", "live"), default="fake")
    parser.add_argument("--base-url", default="http://127.0.0.1:8080",
                        help="Gateway base URL in live mode")
    parser.add_argument("--api-key", default=os.getenv("CHAT_GATEWAY_API_KEY"))
    parser.add_argument("--models", default="auto",
                        help="Comma-separated model IDs; alternation approximates switch latency")
    parser.add_argument("--requests", type=int, default=10)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--pid", type=int,
                        help="Optional gateway PID for an RSS snapshot")
    parser.add_argument("--output", help="Optional JSON output path")
    args = parser.parse_args()
    if args.requests < 1 or args.requests > 10000:
        parser.error("--requests must be between 1 and 10000")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    server: ThreadingHTTPServer | None = None
    if args.mode == "fake":
        server = ThreadingHTTPServer(("127.0.0.1", 0), FakeBackend)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        url = f"http://127.0.0.1:{server.server_port}/v1/chat/completions"
        models = ["fake-a", "fake-b"]
    else:
        url = args.base_url.rstrip("/") + "/v1/chat/completions"
        models = [item.strip() for item in args.models.split(",") if item.strip()]
        if not models:
            parser.error("--models must contain at least one model ID")

    results: list[dict[str, Any]] = []
    previous_model: str | None = None
    try:
        for index in range(args.requests):
            model = models[index % len(models)]
            row = one_request(url, model, args.api_key, args.timeout)
            row.update({
                "request": index + 1,
                "switch_request": previous_model is not None and model != previous_model,
            })
            results.append(row)
            previous_model = model
    finally:
        if server:
            server.shutdown()
            server.server_close()

    ttft = [row["ttft_ms"] for row in results]
    total = [row["total_ms"] for row in results]
    switch = [row["ttft_ms"] for row in results if row["switch_request"]]
    report = {
        "mode": args.mode,
        "endpoint": url,
        "environment": {
            "python": platform.python_version(),
            "platform": platform.platform(),
            "requests": args.requests,
            "models": models,
            "gateway_rss_mb": process_rss_mb(args.pid),
        },
        "summary_ms": {
            "ttft_median": round(statistics.median(ttft), 3),
            "ttft_p95": percentile(ttft, 0.95),
            "total_median": round(statistics.median(total), 3),
            "total_p95": percentile(total, 0.95),
            "switch_ttft_median": round(statistics.median(switch), 3) if switch else None,
        },
        "results": results,
        "notes": [
            "Fake mode measures local HTTP/SSE harness overhead, not provider or gateway performance.",
            "Live switch latency is measured as request-start-to-first-event when the model ID changes.",
            "RSS is a single snapshot for the supplied PID; run repeated samples externally for a memory trend.",
        ],
    }
    rendered = json.dumps(report, indent=2)
    print(rendered)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as stream:
            stream.write(rendered + "\\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
