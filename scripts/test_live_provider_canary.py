import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from live_provider_canary import (
    _NoRedirect, assert_turn_history, consume_sse, preflight,
    read_state, validate_canary_target, write_state,
)


class ConsumeSseTests(unittest.TestCase):
    def test_collects_content_and_requires_done(self):
        events = [
            'data: {"choices":[{"delta":{"role":"assistant"}}]}\n',
            'data: {"choices":[{"delta":{"content":"marker-"}}]}\n',
            'data: {"choices":[{"delta":{"content":"ok"}}]}\n',
            'data: [DONE]\n',
        ]
        result = consume_sse(events)
        self.assertEqual(result["content"], "marker-ok")
        self.assertTrue(result["saw_done"])
        self.assertFalse(result["provider_error"])
        self.assertIsNotNone(result["first_content_ms"])

    def test_ttft_uses_request_dispatch_timestamp(self):
        from unittest.mock import patch

        with patch("live_provider_canary.time.monotonic", return_value=10.25):
            result = consume_sse(
                ['data: {"choices":[{"delta":{"content":"first"}}]}\n'],
                started_at=10.0,
            )
        self.assertEqual(result["first_content_ms"], 250)

    def test_missing_done_is_not_success(self):
        result = consume_sse([
            'data: {"choices":[{"delta":{"content":"partial"}}]}\n',
        ])
        self.assertEqual(result["content"], "partial")
        self.assertFalse(result["saw_done"])

    def test_transcript_assertion_rejects_missing_or_duplicate_turns(self):
        conversation = {
            "messages": [
                {"role": "user", "content": "redacted"},
                {"role": "assistant", "content": "redacted"},
                {"role": "user", "content": "redacted"},
                {"role": "assistant", "content": "redacted"},
            ]
        }
        assert_turn_history(conversation, 2)
        with self.assertRaisesRegex(RuntimeError, "message_count_mismatch"):
            assert_turn_history({"messages": conversation["messages"][:-1]}, 2)
        duplicate = {"messages": conversation["messages"] + [conversation["messages"][-1]]}
        with self.assertRaisesRegex(RuntimeError, "message_count_mismatch"):
            assert_turn_history(duplicate, 2)

    def test_transcript_assertion_rejects_wrong_message_order(self):
        conversation = {
            "messages": [
                {"role": "user"}, {"role": "assistant"},
                {"role": "assistant"}, {"role": "user"},
            ]
        }
        with self.assertRaisesRegex(RuntimeError, "message_order_mismatch"):
            assert_turn_history(conversation, 2)


    def test_restart_state_preserves_two_distinct_conversations(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "canary-state.json"
            write_state(path, "primary-id", "primary-marker", "isolated-id", "isolated-marker")
            self.assertEqual(
                read_state(path),
                ("primary-id", "primary-marker", "isolated-id", "isolated-marker"),
            )
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_restart_state_replaces_symlink_without_touching_target(self):
        import os

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target.json"
            target.write_text("do not overwrite", encoding="utf-8")
            path = root / "canary-state.json"
            try:
                path.symlink_to(target)
            except (OSError, NotImplementedError):
                self.skipTest("symlinks are unavailable on this platform")
            write_state(path, "primary-id", "primary-marker", "isolated-id", "isolated-marker")
            self.assertFalse(path.is_symlink())
            self.assertEqual(target.read_text(encoding="utf-8"), "do not overwrite")
            self.assertEqual(read_state(path)[0], "primary-id")

    def test_restart_state_rejects_symlink_and_oversized_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target.json"
            target.write_text("{}", encoding="utf-8")
            link = root / "link.json"
            try:
                link.symlink_to(target)
            except (OSError, NotImplementedError):
                self.skipTest("symlinks are unavailable on this platform")
            with self.assertRaisesRegex(RuntimeError, "canary_state_missing_or_invalid"):
                read_state(link)

            oversized = root / "oversized.json"
            oversized.write_text(" " * 16_385, encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "canary_state_missing_or_invalid"):
                read_state(oversized)

    def test_restart_state_rejects_reused_conversation_or_marker(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "canary-state.json"
            write_state(path, "same-id", "primary-marker", "same-id", "isolated-marker")
            with self.assertRaisesRegex(RuntimeError, "canary_state_missing_or_invalid"):
                read_state(path)

    def test_provider_error_is_reported_without_exposing_payload(self):
        result = consume_sse([
            'data: {"error":{"message":"sensitive upstream detail"}}\n',
        ])
        self.assertTrue(result["provider_error"])
        self.assertFalse(result["saw_done"])
        self.assertEqual(result["content"], "")


class CanaryPreflightTests(unittest.TestCase):
    def test_keyless_local_run_requires_explicit_opt_in(self):
        validate_canary_target("http://127.0.0.1:8080", "", True)
        validate_canary_target("http://localhost:8080", "", True)
        with self.assertRaisesRegex(RuntimeError, "API_KEY_not_set"):
            validate_canary_target("http://127.0.0.1:8080", "", False)
        with self.assertRaisesRegex(RuntimeError, "API_KEY_not_set"):
            validate_canary_target("https://private.example", "", True)

    def test_rejects_http_remote_and_authority_tricks(self):
        for url in (
            "http://private.example:8080", "http://127.0.0.1:0",
            "http://user:secret@127.0.0.1:8080",
            "http://127.0.0.1:8080/relay/v1", "http://127.0.0.1/?x=1",
            "http://127.0.0.1/#fragment", "ftp://127.0.0.1",
            "http://127.0.0.1:bad",
        ):
            with self.subTest(url=url), self.assertRaisesRegex(RuntimeError, "invalid_gateway_url"):
                validate_canary_target(url, "private-key-not-printed", False)

    def test_preflight_validates_provider_ownership_without_sending_prompts(self):
        class Response:
            status = 200
            def __init__(self, body):
                self.body = json.dumps(body).encode("utf-8")
            def __enter__(self):
                return self
            def __exit__(self, *_):
                return False
            def read(self, _limit):
                return self.body

        class Opener:
            def __init__(self):
                self.paths = []
                self.models = [
                    {"id": "model-" + provider, "owned_by": provider}
                    for provider in ("chatgpt", "claude", "gemini", "grok")
                ]
            def open(self, request, timeout):
                self.paths.append(request.full_url.rsplit("/", 1)[-1])
                if request.full_url.endswith("/health"):
                    return Response({"status": "ok"})
                return Response({"object": "list", "data": self.models})

        opener = Opener()
        models = {f"CANARY_{provider.upper()}_MODEL": "model-" + provider
                  for provider in ("chatgpt", "claude", "gemini", "grok")}
        with patch.dict(os.environ, models), patch(
            "live_provider_canary.urllib.request.build_opener", return_value=opener
        ):
            result = preflight("http://127.0.0.1:8080", "", 5)
            self.assertEqual(result["status"], "passed")
            self.assertEqual(result["prompts_sent"], 0)
            self.assertEqual(opener.paths, ["health", "models"])
            opener.models = [
                {"id": "model-" + provider, "owned_by": "wrong-owner"}
                for provider in ("chatgpt", "claude", "gemini", "grok")
            ]
            with self.assertRaisesRegex(RuntimeError, "models_missing"):
                preflight("http://127.0.0.1:8080", "", 5)

    def test_live_requests_do_not_follow_redirects(self):
        self.assertIsNone(_NoRedirect().redirect_request(
            None, None, 302, "Found", {}, "https://untrusted.example/"
        ))


if __name__ == "__main__":
    unittest.main()
