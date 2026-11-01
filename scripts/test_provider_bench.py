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


if __name__ == "__main__":
    unittest.main()
