import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('benchmark', Path(__file__).with_name('metaai-latency-benchmark.py'))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class StreamMeasurementTests(unittest.TestCase):
    def test_role_and_headers_are_not_first_content_and_tail_is_separate(self):
        m = benchmark.SseMeasurement('OK')
        m.observe(b'data: {"choices":[{"delta":{"role":"assistant"}}]}', 1)
        m.observe(b'data: {"choices":[{"delta":{"content":"O"}}]}', 7)
        m.observe(b'data: {"choices":[{"delta":{"content":"K"}}]}', 8)
        m.observe(b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}', 22)
        m.observe(b'data: [DONE]', 23)
        result = m.result(24)
        self.assertTrue(result['success'])
        self.assertEqual((result['first_content_ms'], result['tail_ms'], result['total_ms']), (7, 15, 24))

    def test_done_is_not_success_for_errors_or_wrong_or_truncated_content(self):
        for content in ['O', 'UNEXPECTED']:
            m = benchmark.SseMeasurement('OK')
            m.observe(('data: {"choices":[{"delta":{"content":"'+content+'"},"finish_reason":"stop"}]}').encode(), 1)
            m.observe(b'data: [DONE]', 2)
            self.assertFalse(m.result(2)['success'])
        m = benchmark.SseMeasurement('OK')
        m.observe(b'data: {"error":{"code":"stream_failed"}}', 1)
        m.observe(b'data: [DONE]', 2)
        self.assertFalse(m.result(2)['success'])

    def test_malformed_oversized_and_post_terminal_events_fail_validation(self):
        for line in [b'data: {"choices":null}', b'data: {"choices":[null]}', b'data: not-json']:
            m = benchmark.SseMeasurement('OK')
            m.observe(line, 1)
            self.assertFalse(m.result(2)['success'])
        m = benchmark.SseMeasurement('OK')
        m.observe(b'data: [DONE]', 1)
        m.observe(b'data: {"choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}', 2)
        self.assertFalse(m.result(3)['success'])
        m = benchmark.SseMeasurement('OK')
        m.observe(('data: {"choices":[{"delta":{"content":"OK'+(' '*5000)+'"},"finish_reason":"stop"}]}').encode(), 1)
        m.observe(b'data: [DONE]', 2)
        self.assertFalse(m.result(3)['success'])

    def test_small_cohort_and_failures_are_visible(self):
        rows = [{'success': True, 'first_content_ms': 2, 'total_ms': 24, 'tail_ms': 15}, {'success': False}]
        result = benchmark.summarize(rows)
        self.assertEqual((result['samples'], result['failures']), (2, 1))
        self.assertFalse(result['p95_minimum_sample_count_met'])
        self.assertEqual(result['total_ms']['p95'], 24)


if __name__ == '__main__':
    unittest.main()
