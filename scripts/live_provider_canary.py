#!/usr/bin/env python3
"""Opt-in live streaming canary for real Chat-Gateway provider sessions.

Run phase 1, restart the gateway while preserving its SQLite and Obscura
storage, then run phase 2. Reports intentionally exclude prompts and content.
"""
from __future__ import annotations

import argparse
import json
import os
import secrets
import stat
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Iterable

CORE_PHASE_TWO = ("claude", "gemini", "grok")


def consume_sse(lines: Iterable[str]) -> dict:
    content: list[str] = []
    first_content_ms = None
    started = time.monotonic()
    saw_done = False
    provider_error = False
    for raw_line in lines:
        line = raw_line.decode("utf-8", "replace") if isinstance(raw_line, bytes) else raw_line
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            saw_done = True
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
        if not choices:
            continue
        delta = choices[0].get("delta") or {}
        piece = delta.get("content")
        if isinstance(piece, str) and piece:
            if first_content_ms is None:
                first_content_ms = int((time.monotonic() - started) * 1000)
            content.append(piece)
    return {
        "content": "".join(content),
        "first_content_ms": first_content_ms,
        "saw_done": saw_done,
        "provider_error": provider_error,
    }


def request_turn(base_url: str, api_key: str, model: str, prompt: str,
                 conversation_id: str | None, timeout: int) -> dict:
    payload = json.dumps({
        "model": model,
        "stream": True,
        "messages": [{"role": "user", "content": prompt}],
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
            returned_conversation_id = response.headers.get("X-Conversation-Id")
            parsed = consume_sse(response)
            status = response.status
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"http_status_{error.code}") from None
    except (urllib.error.URLError, TimeoutError, OSError):
        raise RuntimeError("gateway_unreachable_or_timeout") from None
    elapsed_ms = int((time.monotonic() - started) * 1000)
    if status < 200 or status >= 300:
        raise RuntimeError(f"http_status_{status}")
    if parsed["provider_error"]:
        raise RuntimeError("provider_stream_error")
    if not parsed["content"]:
        raise RuntimeError("empty_provider_response")
    if not parsed["saw_done"]:
        raise RuntimeError("missing_sse_done")
    return {
        "conversation_id": returned_conversation_id,
        "content": parsed["content"],
        "ttft_ms": parsed["first_content_ms"],
        "total_ms": elapsed_ms,
    }


def write_state(path: Path, conversation_id: str, marker: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, stat.S_IRUSR | stat.S_IWUSR)
    with os.fdopen(descriptor, "w", encoding="utf-8") as state_file:
        json.dump({"conversation_id": conversation_id, "marker": marker}, state_file)
        state_file.write("\n")
    os.chmod(path, stat.S_IRUSR | stat.S_IWUSR)


def read_state(path: Path) -> tuple[str, str]:
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
        conversation_id = state["conversation_id"]
        marker = state["marker"]
    except (OSError, KeyError, TypeError, json.JSONDecodeError):
        raise RuntimeError("canary_state_missing_or_invalid") from None
    if not isinstance(conversation_id, str) or not isinstance(marker, str):
        raise RuntimeError("canary_state_missing_or_invalid")
    return conversation_id, marker


def run_phase(phase: str) -> dict:
    base_url = os.environ.get("CHAT_GATEWAY_URL", "http://127.0.0.1:8080")
    api_key = os.environ.get("CHAT_GATEWAY_API_KEY", "")
    state_path = Path(os.environ.get(
        "CHAT_GATEWAY_CANARY_STATE",
        "/tmp/chat-gateway-live-canary-state.json",
    ))
    timeout = int(os.environ.get("CHAT_GATEWAY_CANARY_TIMEOUT", "180"))
    if not api_key:
        raise RuntimeError("CHAT_GATEWAY_API_KEY_not_set")

    results = []
    if phase == "before-restart":
        model = os.environ.get("CANARY_CHATGPT_MODEL")
        if not model:
            raise RuntimeError("CANARY_CHATGPT_MODEL_not_set")
        marker = "CGCANARY-" + secrets.token_hex(8)
        first = request_turn(
            base_url, api_key, model,
            f"Remember this exact continuity marker for this conversation: {marker}. Reply only ACK.",
            None, timeout,
        )
        conversation_id = first["conversation_id"]
        if not conversation_id:
            raise RuntimeError("conversation_id_header_missing")
        results.append({"provider": "chatgpt", "scenario": "initial_turn",
                        "status": "passed", "ttft_ms": first["ttft_ms"],
                        "total_ms": first["total_ms"]})
        follow_up = request_turn(
            base_url, api_key, model,
            "Return the exact continuity marker I asked you to remember. Do not explain.",
            conversation_id, timeout,
        )
        if marker.lower() not in follow_up["content"].lower():
            raise RuntimeError("chatgpt_same_thread_continuity_failed")
        results.append({"provider": "chatgpt", "scenario": "same_provider_follow_up",
                        "status": "passed", "ttft_ms": follow_up["ttft_ms"],
                        "total_ms": follow_up["total_ms"]})
        write_state(state_path, conversation_id, marker)
        return {"phase": phase, "status": "passed", "results": results,
                "next_step": "Restart Chat-Gateway without deleting SQLite or Obscura session storage, then run --phase after-restart."}

    conversation_id, marker = read_state(state_path)
    for provider in CORE_PHASE_TWO:
        model = os.environ.get(f"CANARY_{provider.upper()}_MODEL")
        if not model:
            raise RuntimeError(f"CANARY_{provider.upper()}_MODEL_not_set")
        response = request_turn(
            base_url, api_key, model,
            "Across provider changes and a gateway restart, return the exact continuity marker "
            "from earlier in this conversation. Do not guess; if unavailable, say UNKNOWN.",
            conversation_id, timeout,
        )
        if marker.lower() not in response["content"].lower():
            raise RuntimeError(f"{provider}_cross_provider_continuity_failed")
        results.append({"provider": provider, "scenario": "post_restart_cross_provider_continuity",
                        "status": "passed", "ttft_ms": response["ttft_ms"],
                        "total_ms": response["total_ms"]})
    try:
        state_path.unlink()
    except FileNotFoundError:
        pass
    return {"phase": phase, "status": "passed", "results": results}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", required=True,
                        choices=("before-restart", "after-restart"))
    args = parser.parse_args()
    try:
        report = run_phase(args.phase)
    except RuntimeError as error:
        report = {"phase": args.phase, "status": "failed",
                  "failure_category": str(error)}
    print(json.dumps(report, sort_keys=True))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
