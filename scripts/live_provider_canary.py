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
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Iterable

CORE_PHASE_TWO = ("claude", "gemini", "grok")


def consume_sse(lines: Iterable[str], started_at: float | None = None) -> dict:
    content: list[str] = []
    first_content_ms = None
    started = time.monotonic() if started_at is None else started_at
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


def request_turn(base_url: str, api_key: str, model: str, expected_provider: str,
                 prompt: str, conversation_id: str | None, timeout: int) -> dict:
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
            returned_provider = response.headers.get("X-Gateway-Provider", "").lower()
            parsed = consume_sse(response, started)
            status = response.status
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"http_status_{error.code}") from None
    except (urllib.error.URLError, TimeoutError, OSError):
        raise RuntimeError("gateway_unreachable_or_timeout") from None
    elapsed_ms = int((time.monotonic() - started) * 1000)
    if status < 200 or status >= 300:
        raise RuntimeError(f"http_status_{status}")
    if returned_provider != expected_provider:
        raise RuntimeError("provider_identity_header_missing_or_mismatched")
    if conversation_id and returned_conversation_id != conversation_id:
        raise RuntimeError("conversation_id_changed_during_follow_up")
    if parsed["provider_error"]:
        raise RuntimeError("provider_stream_error")
    if not parsed["content"]:
        raise RuntimeError("empty_provider_response")
    if not parsed["saw_done"]:
        raise RuntimeError("missing_sse_done")
    return {
        "conversation_id": returned_conversation_id,
        "provider": returned_provider,
        "content": parsed["content"],
        "ttft_ms": parsed["first_content_ms"],
        "total_ms": elapsed_ms,
    }


def fetch_conversation(base_url: str, api_key: str, conversation_id: str, timeout: int) -> dict:
    request = urllib.request.Request(
        base_url.rstrip("/") + "/v1/conversations/" + conversation_id,
        headers={"Authorization": f"Bearer {api_key}", "Accept": "application/json"},
        method="GET",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            if response.status != 200:
                raise RuntimeError(f"conversation_export_http_status_{response.status}")
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"conversation_export_http_status_{error.code}") from None
    except (urllib.error.URLError, TimeoutError, OSError, json.JSONDecodeError):
        raise RuntimeError("conversation_export_failed") from None


def assert_turn_history(conversation: dict, expected_turns: int) -> None:
    messages = conversation.get("messages")
    if not isinstance(messages, list) or len(messages) != expected_turns * 2:
        raise RuntimeError("conversation_message_count_mismatch")
    expected_roles = ["user", "assistant"] * expected_turns
    actual_roles = [message.get("role") for message in messages if isinstance(message, dict)]
    if actual_roles != expected_roles:
        raise RuntimeError("conversation_message_order_mismatch")


def write_state(
    path: Path,
    conversation_id: str,
    marker: str,
    isolated_conversation_id: str,
    isolated_marker: str,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # Write a private temporary file and atomically replace the destination.
    # os.replace replaces a destination symlink itself rather than following it.
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.",
        suffix=".tmp",
        dir=path.parent,
    )
    temporary_path = Path(temporary_name)
    try:
        os.fchmod(descriptor, stat.S_IRUSR | stat.S_IWUSR)
        with os.fdopen(descriptor, "w", encoding="utf-8") as state_file:
            json.dump({
                "conversation_id": conversation_id,
                "marker": marker,
                "isolated_conversation_id": isolated_conversation_id,
                "isolated_marker": isolated_marker,
            }, state_file)
            state_file.write("\\n")
            state_file.flush()
            os.fsync(state_file.fileno())
        os.replace(temporary_path, path)
        # Persist the directory entry where directory fsync is supported.
        directory_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
        try:
            directory_fd = os.open(path.parent, directory_flags)
        except OSError:
            directory_fd = None
        if directory_fd is not None:
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
    finally:
        try:
            temporary_path.unlink()
        except FileNotFoundError:
            pass


def read_state(path: Path) -> tuple[str, str, str, str]:
    try:
        # O_NOFOLLOW plus fstat checks the opened inode, avoiding an lstat/read
        # race in which an attacker swaps the state path for a symlink.
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(path, flags)
        with os.fdopen(descriptor, "rb") as state_file:
            metadata = os.fstat(state_file.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 16_384:
                raise RuntimeError("canary_state_missing_or_invalid")
            encoded = state_file.read(16_385)
            if len(encoded) > 16_384:
                raise RuntimeError("canary_state_missing_or_invalid")
        state = json.loads(encoded.decode("utf-8"))
        values = (
            state["conversation_id"], state["marker"],
            state["isolated_conversation_id"], state["isolated_marker"],
        )
    except (OSError, KeyError, TypeError, UnicodeDecodeError, json.JSONDecodeError):
        raise RuntimeError("canary_state_missing_or_invalid") from None
    if not all(isinstance(value, str) and value for value in values):
        raise RuntimeError("canary_state_missing_or_invalid")
    if values[0] == values[2] or values[1] == values[3]:
        raise RuntimeError("canary_state_missing_or_invalid")
    return values


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
        isolated_marker = "CGCANARY-ISOLATED-" + secrets.token_hex(8)

        first = request_turn(
            base_url, api_key, model, "chatgpt",
            f"Remember this exact continuity marker for this conversation: {marker}. Reply only ACK.",
            None, timeout,
        )
        conversation_id = first["conversation_id"]
        if not conversation_id:
            raise RuntimeError("conversation_id_header_missing")
        results.append({"provider": "chatgpt", "scenario": "primary_initial_turn",
                        "status": "passed", "ttft_ms": first["ttft_ms"],
                        "total_ms": first["total_ms"]})

        isolated_first = request_turn(
            base_url, api_key, model, "chatgpt",
            f"Remember this different exact marker only in this separate conversation: {isolated_marker}. Reply only ACK.",
            None, timeout,
        )
        isolated_conversation_id = isolated_first["conversation_id"]
        if not isolated_conversation_id or isolated_conversation_id == conversation_id:
            raise RuntimeError("isolated_conversation_id_missing_or_reused")
        results.append({"provider": "chatgpt", "scenario": "interleaved_isolated_initial_turn",
                        "status": "passed", "ttft_ms": isolated_first["ttft_ms"],
                        "total_ms": isolated_first["total_ms"]})

        follow_up = request_turn(
            base_url, api_key, model, "chatgpt",
            "Return the exact continuity marker I asked you to remember. Do not explain.",
            conversation_id, timeout,
        )
        if marker.lower() not in follow_up["content"].lower():
            raise RuntimeError("chatgpt_same_thread_continuity_failed")
        assert_turn_history(fetch_conversation(base_url, api_key, conversation_id, timeout), 2)
        results.append({"provider": "chatgpt", "scenario": "primary_same_provider_follow_up",
                        "status": "passed", "ttft_ms": follow_up["ttft_ms"],
                        "total_ms": follow_up["total_ms"]})

        isolated_follow_up = request_turn(
            base_url, api_key, model, "chatgpt",
            "Return the exact marker from this separate conversation. Do not guess.",
            isolated_conversation_id, timeout,
        )
        if isolated_marker.lower() not in isolated_follow_up["content"].lower():
            raise RuntimeError("interleaved_conversation_continuity_failed")
        if marker.lower() in isolated_follow_up["content"].lower():
            raise RuntimeError("conversation_context_leaked_between_threads")
        assert_turn_history(
            fetch_conversation(base_url, api_key, isolated_conversation_id, timeout), 2
        )
        results.append({"provider": "chatgpt", "scenario": "interleaved_isolated_follow_up",
                        "status": "passed", "ttft_ms": isolated_follow_up["ttft_ms"],
                        "total_ms": isolated_follow_up["total_ms"]})

        write_state(
            state_path, conversation_id, marker,
            isolated_conversation_id, isolated_marker,
        )
        return {"phase": phase, "status": "passed", "results": results,
                "next_step": "Restart Chat-Gateway without deleting SQLite or Obscura session storage, then run --phase after-restart."}

    conversation_id, marker, isolated_conversation_id, isolated_marker = read_state(state_path)
    for provider in CORE_PHASE_TWO:
        model = os.environ.get(f"CANARY_{provider.upper()}_MODEL")
        if not model:
            raise RuntimeError(f"CANARY_{provider.upper()}_MODEL_not_set")
        response = request_turn(
            base_url, api_key, model, provider,
            "Across provider changes and a gateway restart, return the exact continuity marker "
            "from earlier in this conversation. Do not guess; if unavailable, say UNKNOWN.",
            conversation_id, timeout,
        )
        if marker.lower() not in response["content"].lower():
            raise RuntimeError(f"{provider}_cross_provider_continuity_failed")
        expected_turns = 2 + CORE_PHASE_TWO.index(provider) + 1
        assert_turn_history(fetch_conversation(base_url, api_key, conversation_id, timeout), expected_turns)
        results.append({"provider": provider, "scenario": "post_restart_cross_provider_continuity",
                        "status": "passed", "ttft_ms": response["ttft_ms"],
                        "total_ms": response["total_ms"]})
    chatgpt_model = os.environ.get("CANARY_CHATGPT_MODEL")
    if not chatgpt_model:
        raise RuntimeError("CANARY_CHATGPT_MODEL_not_set")
    isolated = request_turn(
        base_url, api_key, chatgpt_model, "chatgpt",
        "Return the exact marker from this separate conversation after the gateway restart. Do not guess.",
        isolated_conversation_id, timeout,
    )
    if isolated_marker.lower() not in isolated["content"].lower():
        raise RuntimeError("isolated_conversation_lost_after_restart")
    if marker.lower() in isolated["content"].lower():
        raise RuntimeError("primary_context_leaked_into_isolated_conversation")
    assert_turn_history(
        fetch_conversation(base_url, api_key, isolated_conversation_id, timeout), 3
    )
    results.append({"provider": "chatgpt", "scenario": "isolated_conversation_after_restart",
                    "status": "passed", "ttft_ms": isolated["ttft_ms"],
                    "total_ms": isolated["total_ms"]})
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
