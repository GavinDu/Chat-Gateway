import unittest
import urllib.error
from unittest.mock import patch

from provider_switch_benchmark import consume_sse, latency_category, measure_sequence, parse_step, percentile, request_turn, summarize
from provider_bench import _NoRedirect


class ProviderSwitchBenchmarkTests(unittest.TestCase):
    def test_switch_requests_never_replay_gateway_credentials_across_redirects(self):
        with patch("provider_switch_benchmark.urllib.request.build_opener") as factory:
            factory.return_value.open.side_effect = urllib.error.HTTPError(
                "https://private.example/v1/chat/completions", 308,
                "redirect refused", {}, None
            )
            result = request_turn(
                "https://private.example", "synthetic-secret",
                "chatgpt", "gpt", "synthetic-conversation", 0, 5,
            )
        self.assertEqual(result, {"ok": False, "failure": "http_308"})
        self.assertIsInstance(factory.call_args.args[0], _NoRedirect)
        request = factory.return_value.open.call_args.args[0]
        self.assertEqual(request.get_header("Authorization"), "Bearer synthetic-secret")
        self.assertEqual(request.get_header("X-conversation-id"), "synthetic-conversation")

    def test_switch_request_rejects_unsafe_endpoints_without_sending_credentials(self):
        with patch("provider_switch_benchmark.urllib.request.build_opener") as factory:
            for url in [
                "http://private.example",
                "https://user:password@private.example",
                "https://private.example/extra-path",
                "https://private.example?secret=x",
            ]:
                with self.subTest(url=url):
                    result = request_turn(
                        url, "synthetic-secret", "chatgpt", "gpt", None, 0, 5,
                    )
                    self.assertEqual(
                        result, {"ok": False, "failure": "invalid_benchmark_endpoint"},
                    )
            factory.assert_not_called()

    def test_step_requires_provider_and_model(self):
        self.assertEqual(parse_step("chatgpt=gpt-model"), ("chatgpt", "gpt-model"))
        with self.assertRaises(Exception):
            parse_step("chatgpt")

    def test_percentiles_use_nearest_rank(self):
        self.assertEqual(percentile([50, 10, 40, 20, 30], 50), 30)
        self.assertEqual(percentile([50, 10, 40, 20, 30], 95), 50)
        self.assertIsNone(percentile([], 95))
        self.assertEqual(summarize([10, 20]), {
            "samples": 2, "p50_ms": 10, "p95_ms": 20,
        })

    def test_failed_prior_step_does_not_change_sequence_classification(self):
        self.assertEqual(latency_category(None, "chatgpt"), "initial_provider_request")
        self.assertEqual(latency_category("chatgpt", "claude"), "after_provider_switch")
        self.assertEqual(latency_category("claude", "claude"), "same_provider_follow_up")

    def test_switches_and_followups_reuse_one_conversation_per_repetition(self):
        observed = []

        def fake_turn(base_url, api_key, provider, model, conversation_id, number, timeout):
            observed.append((provider, conversation_id))
            return {
                "ok": True,
                "conversation_id": conversation_id or f"synthetic-conversation-{number // 3}",
                "ttft_ms": 10,
                "total_ms": 25,
            }

        with patch("provider_switch_benchmark.request_turn", side_effect=fake_turn):
            categories, failures = measure_sequence(
                "http://127.0.0.1:8080", "synthetic-key",
                [("chatgpt", "gpt"), ("claude", "sonnet"), ("chatgpt", "gpt")],
                2, 5,
            )
        self.assertEqual(observed, [
            ("chatgpt", None),
            ("claude", "synthetic-conversation-0"),
            ("chatgpt", "synthetic-conversation-0"),
            ("chatgpt", None),
            ("claude", "synthetic-conversation-1"),
            ("chatgpt", "synthetic-conversation-1"),
        ])
        self.assertEqual(failures, {})
        self.assertEqual(len(categories["initial_provider_request"]), 2)
        self.assertEqual(len(categories["after_provider_switch"]), 4)

    def test_failed_handoff_keeps_previous_conversation_for_next_provider(self):
        observed = []
        def fake_turn(base_url, api_key, provider, model, conversation_id, number, timeout):
            observed.append((provider, conversation_id))
            if number == 1:
                return {"ok": False, "failure": "provider_stream_error"}
            return {
                "ok": True,
                "conversation_id": conversation_id or "synthetic-conversation",
                "ttft_ms": 8,
                "total_ms": 20,
            }

        with patch("provider_switch_benchmark.request_turn", side_effect=fake_turn):
            categories, failures = measure_sequence(
                "http://127.0.0.1:8080", "synthetic-key",
                [("chatgpt", "gpt"), ("claude", "sonnet"), ("gemini", "flash")],
                1, 5,
            )
        self.assertEqual(observed, [
            ("chatgpt", None),
            ("claude", "synthetic-conversation"),
            ("gemini", "synthetic-conversation"),
        ])
        self.assertEqual(failures, {"claude:provider_stream_error": 1})
        self.assertEqual(len(categories["after_provider_switch"]), 1)

    def test_sse_measures_first_content_and_terminal(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    'data: {"choices":[{"delta":{"role":"assistant"}}]}',
                    'data: {"choices":[{"delta":{"content":"hello"}}]}',
                    'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}',
                    "data: [DONE]",
                ])
        with patch("provider_concurrency_benchmark.time.monotonic", side_effect=[1.25]):
            ttft, done, error, finished = consume_sse(FakeResponse(), 1.0)
        self.assertEqual(ttft, 250)
        self.assertTrue(done)
        self.assertFalse(error)
        self.assertTrue(finished)

    def test_sse_rejects_error_and_missing_done(self):
        class FakeResponse:
            def __init__(self, lines):
                self.lines = lines
            def __iter__(self):
                return iter(self.lines)
        _, done, error, finished = consume_sse(
            FakeResponse(['data: {"error":{"message":"redacted"}}']), 1.0
        )
        self.assertFalse(done)
        self.assertTrue(error)
        self.assertFalse(finished)
        ttft, done, error, finished = consume_sse(
            FakeResponse(['data: {"choices":[{"delta":{"content":"partial"}}]}']), 1.0
        )
        self.assertIsNotNone(ttft)
        self.assertFalse(done)
        self.assertFalse(error)
        self.assertFalse(finished)

    def test_missing_stop_and_truncated_responses_are_not_successful(self):
        import time
        class FakeResponse:
            def __init__(self, lines):
                self.lines = lines
            def __iter__(self):
                return iter(self.lines)

        prefix = ['data: {"choices":[{"delta":{"content":"partial"}}]}']
        ttft, done, error, finished = consume_sse(
            FakeResponse(prefix + ['data: [DONE]']), time.monotonic()
        )
        self.assertIsNotNone(ttft)
        self.assertTrue(done)
        self.assertFalse(error)
        self.assertFalse(finished)

        ttft, done, error, finished = consume_sse(
            FakeResponse(prefix +
                ['data: {"choices":[{"delta":{},"finish_reason":"length"}]}',
                 'data: [DONE]']), time.monotonic()
        )
        self.assertIsNotNone(ttft)
        self.assertTrue(error)
        self.assertFalse(finished)


if __name__ == "__main__":
    unittest.main()
