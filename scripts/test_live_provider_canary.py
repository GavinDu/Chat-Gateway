import unittest

from live_provider_canary import consume_sse


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

    def test_provider_error_is_reported_without_exposing_payload(self):
        result = consume_sse([
            'data: {"error":{"message":"sensitive upstream detail"}}\n',
        ])
        self.assertTrue(result["provider_error"])
        self.assertFalse(result["saw_done"])
        self.assertEqual(result["content"], "")


if __name__ == "__main__":
    unittest.main()
