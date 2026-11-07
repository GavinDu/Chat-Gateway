import unittest
import urllib.error
from types import SimpleNamespace
from unittest.mock import patch

from provider_bench import (
    _NoRedirect, MAX_SSE_LINE_BYTES, consume_live_benchmark_sse,
    has_content_delta, live_trial, validate_benchmark_target,
)


class ProviderBenchmarkSseTests(unittest.TestCase):
    def test_role_only_event_is_not_first_token(self):
        self.assertFalse(has_content_delta(
            b'data: {"choices":[{"delta":{"role":"assistant"},"index":0}]}'
        ))

    def test_nonempty_content_delta_is_first_token(self):
        self.assertTrue(has_content_delta(
            b'data: {"choices":[{"delta":{"content":"hello"},"index":0}]}'
        ))

    def test_empty_content_and_done_are_not_tokens(self):
        self.assertFalse(has_content_delta(
            'data: {"choices":[{"delta":{"content":""},"index":0}]}'
        ))
        self.assertFalse(has_content_delta("data: [DONE]"))

    def test_non_data_and_malformed_events_are_ignored(self):
        self.assertFalse(has_content_delta(": keep-alive"))
        self.assertFalse(has_content_delta("data: not-json"))
        self.assertFalse(has_content_delta(None))

    def test_valid_json_with_unexpected_top_level_shapes_is_ignored(self):
        for payload in ("null", "[]", '"text"', "42", '{"choices":null}',
                        '{"choices":[]}', '{"choices":[null]}',
                        '{"choices":[{"delta":null}]}',
                        '{"choices":[{"delta":"text"}]}',
                        '{"choices":[{"delta":{"content":42}}]}'):
            with self.subTest(payload=payload):
                self.assertFalse(has_content_delta("data: " + payload))

    def test_unexpected_content_shapes_do_not_crash(self):
        for payload in ({"choices": "invalid"}, {"choices": [1]},
                        {"choices": [{"delta": []}]},
                        {"choices": [{"delta": {"content": ["not", "text"]}}]}):
            import json
            with self.subTest(payload=payload):
                self.assertFalse(has_content_delta("data: " + json.dumps(payload)))



class ProviderBenchmarkLiveSecurityTests(unittest.TestCase):
    def test_target_rejects_remote_http_and_authority_or_path_tricks(self):
        validate_benchmark_target("http://127.0.0.1:8080/v1")
        validate_benchmark_target("https://private.example/v1/")
        for url in (
            "http://private.example/v1",
            "http://user:token@127.0.0.1:8080/v1",
            "http://127.0.0.1:8080/v1?token=secret",
            "http://127.0.0.1:8080/v1#fragment",
            "http://127.0.0.1:8080:bad/v1",
            "https://private.example/v1/subpath",
            "https://private.example/",
            "http://127.0.0.1:0/v1",
        ):
            with self.subTest(url=url), self.assertRaisesRegex(
                RuntimeError, "invalid_benchmark_endpoint"
            ):
                validate_benchmark_target(url)

    def test_benchmark_credential_requests_never_follow_redirects(self):
        self.assertIsNone(_NoRedirect().redirect_request(
            None, None, 307, "Temporary Redirect", {},
            "https://untrusted.example/steal"
        ))
        args = SimpleNamespace(
            endpoint="https://private.example/v1",
            api_key="private-secret-do-not-print",
            model="chatgpt/model",
            prompt="synthetic prompt",
            timeout=5,
        )
        class RedirectingOpener:
            def open(self, request, timeout):
                raise urllib.error.HTTPError(
                    request.full_url, 302, "redirect", {}, None
                )
        with patch(
            "provider_bench.urllib.request.build_opener",
            return_value=RedirectingOpener(),
        ), self.assertRaisesRegex(RuntimeError, "benchmark_http_status_302") as error:
            live_trial(args)
        self.assertNotIn(args.api_key, str(error.exception))

    def test_benchmark_counts_ttft_only_after_a_complete_stream(self):
        class FakeResponse:
            def __init__(self, lines):
                self.lines = iter(lines)
            def readline(self, maximum):
                return next(self.lines, b"")[:maximum]

        content = b'data: {"choices":[{"delta":{"content":"first"}}]}\n'
        done = b"data: [DONE]\n"
        ttft = consume_live_benchmark_sse(FakeResponse([content, done]), 0)
        self.assertGreaterEqual(ttft, 0)
        with self.assertRaisesRegex(RuntimeError, "benchmark_incomplete_stream"):
            consume_live_benchmark_sse(FakeResponse([content]), 0)
        with self.assertRaisesRegex(RuntimeError, "benchmark_provider_stream_failed"):
            consume_live_benchmark_sse(FakeResponse([
                content, b'data: {"error":{"code":"provider_failed"}}\n', done,
            ]), 0)

    def test_benchmark_rejects_oversized_stream_line(self):
        class OversizedResponse:
            def readline(self, maximum):
                self_requested_limit = MAX_SSE_LINE_BYTES + 1
                assert maximum == self_requested_limit
                return b"x" * maximum
        with self.assertRaisesRegex(RuntimeError, "benchmark_sse_line_too_large"):
            consume_live_benchmark_sse(OversizedResponse(), 0)


if __name__ == "__main__":
    unittest.main()
