import unittest
from unittest.mock import patch

from provider_switch_benchmark import consume_sse, parse_step, percentile, summarize


class ProviderSwitchBenchmarkTests(unittest.TestCase):
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

    def test_sse_measures_first_content_and_terminal(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    'data: {"choices":[{"delta":{"role":"assistant"}}]}',
                    'data: {"choices":[{"delta":{"content":"hello"}}]}',
                    "data: [DONE]",
                ])
        with patch("provider_switch_benchmark.time.monotonic", side_effect=[1.25]):
            ttft, done, error = consume_sse(FakeResponse(), 1.0)
        self.assertEqual(ttft, 250)
        self.assertTrue(done)
        self.assertFalse(error)

    def test_sse_rejects_error_and_missing_done(self):
        class FakeResponse:
            def __init__(self, lines):
                self.lines = lines
            def __iter__(self):
                return iter(self.lines)
        _, done, error = consume_sse(
            FakeResponse(['data: {"error":{"message":"redacted"}}']), 1.0
        )
        self.assertFalse(done)
        self.assertTrue(error)
        ttft, done, error = consume_sse(
            FakeResponse(['data: {"choices":[{"delta":{"content":"partial"}}]}']), 1.0
        )
        self.assertIsNotNone(ttft)
        self.assertFalse(done)
        self.assertFalse(error)


if __name__ == "__main__":
    unittest.main()
