import unittest

from provider_concurrency_benchmark import parse_sse, peak_rss_kb, percentile


class ProviderConcurrencyBenchmarkTests(unittest.TestCase):
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
            'data: [DONE]',
        ]
        ttft, done, error = parse_sse(lines, started)
        self.assertIsNotNone(ttft)
        self.assertGreaterEqual(ttft, 0)
        self.assertTrue(done)
        self.assertFalse(error)

    def test_sse_rejects_provider_error_and_missing_done(self):
        import time
        started = time.monotonic()
        ttft, done, error = parse_sse(
            ['data: {"error":{"message":"redacted"}}'], started
        )
        self.assertIsNone(ttft)
        self.assertFalse(done)
        self.assertTrue(error)

        ttft, done, error = parse_sse(
            ['data: {"choices":[{"delta":{"content":"hello"}}]}'], started
        )
        self.assertIsNotNone(ttft)
        self.assertFalse(done)
        self.assertFalse(error)


if __name__ == "__main__":
    unittest.main()
