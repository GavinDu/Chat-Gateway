import unittest
import urllib.error
from unittest.mock import patch

from provider_concurrency_benchmark import one_request, parse_sse, peak_rss_kb, percentile
from provider_bench import _NoRedirect


class ProviderConcurrencyBenchmarkTests(unittest.TestCase):
    def test_credential_bearing_requests_do_not_follow_http_redirects(self):
        with patch("provider_concurrency_benchmark.urllib.request.build_opener") as factory:
            opener = factory.return_value
            opener.open.side_effect = urllib.error.HTTPError(
                "https://private.example/v1/chat/completions", 307,
                "redirect refused", {}, None
            )
            result = one_request(
                "https://private.example", "synthetic-api-key",
                "chatgpt", "gpt", 3, 0
            )
        self.assertEqual(result, {"ok": False, "failure": "http_307"})
        self.assertIsInstance(factory.call_args.args[0], _NoRedirect)
        request = opener.open.call_args.args[0]
        self.assertEqual(request.full_url, "https://private.example/v1/chat/completions")
        self.assertEqual(request.get_header("Authorization"), "Bearer synthetic-api-key")

    def test_invalid_benchmark_targets_rejected_before_opening_request(self):
        with patch("provider_concurrency_benchmark.urllib.request.build_opener") as factory:
            for url in [
                "http://private.example",
                "https://user:password@private.example",
                "https://private.example/v1",
                "https://private.example?key=secret",
                "http://127.0.0.1:8080/#fragment",
            ]:
                with self.subTest(url=url):
                    result = one_request(url, "synthetic-key", "chatgpt", "gpt", 3, 0)
                    self.assertEqual(
                        result, {"ok": False, "failure": "invalid_benchmark_endpoint"}
                    )
            factory.assert_not_called()

    def test_percentile_uses_nearest_rank(self):
        values = [50, 10, 40, 20, 30]
        self.assertEqual(percentile(values, 50), 30)
        self.assertEqual(percentile(values, 95), 50)
        self.assertIsNone(percentile([], 95))

    def test_peak_rss_ignores_missing_samples(self):
        self.assertEqual(peak_rss_kb([100, None, 250, 180]), 250)
        self.assertIsNone(peak_rss_kb([None, None]))

    def test_sse_records_first_content_and_terminal_event(self):
        import time
        started = time.monotonic() - 0.01
        lines = [
            'data: {"choices":[{"delta":{"role":"assistant"}}]}',
            'data: {"choices":[{"delta":{"content":"hello"}}]}',
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}',
            'data: [DONE]',
        ]
        ttft, done, error, finished = parse_sse(lines, started)
        self.assertIsNotNone(ttft)
        self.assertGreaterEqual(ttft, 0)
        self.assertTrue(done)
        self.assertFalse(error)
        self.assertTrue(finished)

    def test_sse_rejects_provider_error_and_missing_done(self):
        import time
        started = time.monotonic()
        ttft, done, error, finished = parse_sse(
            ['data: {"error":{"message":"redacted"}}'], started
        )
        self.assertIsNone(ttft)
        self.assertFalse(done)
        self.assertTrue(error)
        self.assertFalse(finished)

        ttft, done, error, finished = parse_sse(
            ['data: {"choices":[{"delta":{"content":"hello"}}]}'], started
        )
        self.assertIsNotNone(ttft)
        self.assertFalse(done)
        self.assertFalse(error)
        self.assertFalse(finished)

    def test_sse_detects_done_without_successful_terminal_chunk(self):
        import time
        started = time.monotonic()
        prefix = ['data: {"choices":[{"delta":{"content":"hello"}}]}']
        for suffix in [
            ['data: [DONE]'],
            ['data: {"error":{"code":"stream_failed"}}', 'data: [DONE]'],
        ]:
            ttft, done, error, finished = parse_sse(prefix + suffix, started)
            self.assertIsNotNone(ttft)
            self.assertFalse(finished)
            self.assertTrue(error or done)

        ttft, done, error, finished = parse_sse(
            prefix + ['data: {"choices":[{"delta":{},"finish_reason":"length"}]}',
                      'data: [DONE]'], started
        )
        self.assertTrue(error)
        self.assertFalse(finished)

    def test_malformed_sse_events_fail_without_exposing_provider_text(self):
        import time
        started = time.monotonic()
        for malformed in ['null', '[]', '{"choices":"not-an-array"}',
                          '{"choices":[null]}', '{"choices":[{"delta":5}]}']:
            ttft, done, error, finished = parse_sse(
                ['data: ' + malformed], started
            )
            self.assertIsNone(ttft)
            self.assertFalse(done)
            self.assertTrue(error)
            self.assertFalse(finished)


if __name__ == "__main__":
    unittest.main()
