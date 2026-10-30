import unittest

from provider_bench import has_content_delta


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


if __name__ == "__main__":
    unittest.main()
