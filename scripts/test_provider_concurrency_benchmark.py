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
