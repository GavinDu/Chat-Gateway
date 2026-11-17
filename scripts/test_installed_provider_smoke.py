"""Only local synthetic fixtures; never submit provider requests in Actions."""
import io
import json
import sys
import unittest
import urllib.error
import uuid
from contextlib import redirect_stdout
from unittest.mock import patch

from installed_provider_smoke import (
    PROVIDERS, SmokeFailure, _identity, _safe_error, execute, main,
    model_inventory, run_matrix, validate_json, validate_sse,
)
from live_provider_canary import consume_sse


class Headers(dict):
    pass


def sample_headers(provider="chatgpt", conversation_id="fixture"):
    return Headers({
        "X-Gateway-Provider": provider,
        "X-Conversation-Id": conversation_id,
        "X-Request-Id": str(uuid.uuid4()),
    })


def answer(content="OK", finish="stop"):
    return {"choices": [{
        "message": {"role": "assistant", "content": content},
        "finish_reason": finish,
    }]}


class ParserContractTests(unittest.TestCase):
    def test_json_requires_exact_marked_finish_and_matching_request(self):
        headers = sample_headers()
        validate_json(answer(), headers, "chatgpt", "fixture")
        for payload, code in [
            (answer("old answer"), "unexpected_answer"),
            (answer("OK "), "unexpected_answer"),
            (answer("OK", "length"), "incomplete_finish"),
            ({"choices": []}, "invalid_response"),
        ]:
            with self.subTest(code=code), self.assertRaises(SmokeFailure) as error:
                validate_json(payload, headers, "chatgpt", "fixture")
            self.assertEqual(error.exception.category, code)

        for header_patch, category in [
            ({"X-Gateway-Provider": "metaai"}, "wrong_provider"),
            ({"X-Conversation-Id": "previous"}, "wrong_conversation"),
            ({"X-Request-Id": "secret-auth-payload"}, "missing_request_id"),
        ]:
            altered = sample_headers()
            altered.update(header_patch)
            with self.subTest(category=category), self.assertRaises(SmokeFailure) as error:
                validate_json(answer(), altered, "chatgpt", "fixture")
            self.assertEqual(error.exception.category, category)

    def test_sse_rejects_partial_error_missing_done_and_wrong_markers(self):
        base = [
            'data: {"choices":[{"delta":{"content":"OK"}}]}\n',
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n',
        ]
        headers = sample_headers()
        parsed = consume_sse(base + ["data: [DONE]\n"])
        validate_sse(parsed, headers, "chatgpt", "fixture")
        for rows in [
            base,  # missing DONE
            [base[0], "data: [DONE]\n"],  # no normal stop
            ['data: {"error":{"message":"private"}}\n'],  # upstream error
            ['data: {"choices":[{"delta":{"content":"old"}}]}\n',
             base[1], "data: [DONE]\n"],  # stale response
        ]:
            with self.assertRaises(SmokeFailure):
                validate_sse(consume_sse(rows), headers, "chatgpt", "fixture")

    def test_inventory_enforces_advertised_provider_ownership(self):
        inventory = model_inventory({"data": [
            {"owned_by": "chatgpt", "id": "chatgpt/gpt"},
            {"owned_by": "metaai", "id": "metaai/meta-ai"},
            {"owned_by": "chatgpt", "id": "duplicate"},
            {"owned_by": "claude", "id": "x" * 300},
            {"owned_by": "untrusted", "id": "mistral/mistral"},
        ]})
        self.assertEqual(inventory, {
            "chatgpt": "chatgpt/gpt", "metaai": "metaai/meta-ai",
        })
        with self.assertRaises(SmokeFailure):
            model_inventory({"data": "wrong"})

    def test_reports_only_supported_sha_and_categories(self):
        self.assertEqual(_identity({"status": "ok", "build": {
            "source_sha": "a" * 40, "credential": "secret"}}),
            {"health": "ok", "source_sha": "a" * 40})
        self.assertEqual(_identity({"status": "error", "source_sha": "url?token=secret"}),
                         {"health": "unverified"})
        self.assertEqual(_safe_error(urllib.error.HTTPError(
            "https://secret.invalid/path?token=private", 502,
            "private detail", None, None)), "http_502")
        self.assertEqual(_safe_error(RuntimeError("secret upstream body")), "invalid_response")


class FakeResponse:
    status = 200

    def __init__(self, body, headers=None):
        self.body = body
        self.pos = 0
        self.headers = headers or Headers()

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def read(self, maximum):
        chunk = self.body[self.pos:self.pos + maximum]
        self.pos += len(chunk)
        return chunk

    def readline(self, maximum):
        i = self.body.find(b"\n", self.pos)
        end = len(self.body) if i < 0 else i + 1
        chunk = self.body[self.pos:min(end, self.pos + maximum)]
        self.pos += len(chunk)
        return chunk


class FakeGateway:
    def __init__(self):
        self.posts = []

    def open(self, req, timeout):
        assert timeout == 8
        if req.full_url.endswith("/health"):
            return FakeResponse(b'{"status":"ok"}')
        if req.full_url.endswith("/v1/models"):
            models = [{"id": f"{p}/model-{p}", "owned_by": p}
                      for p in PROVIDERS]
            return FakeResponse(json.dumps({"data": models}).encode())
        if not req.full_url.endswith("/v1/chat/completions"):
            raise AssertionError("unexpected request")
        content = json.loads(req.data)
        model = content["model"]
        provider = model.split("/")[0]
        self.posts.append((provider, content["stream"]))
        req_id = req.get_header("X-conversation-id")
        assert isinstance(req_id, str)
        headers = sample_headers(provider, req_id)
        if content["stream"]:
            data = (b'data: {"choices":[{"delta":{"content":"OK"}}]}\n'
                    b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n'
                    b'data: [DONE]\n')
        else:
            data = json.dumps(answer()).encode()
        return FakeResponse(data, headers)


class ExecutionContractTests(unittest.TestCase):
    def test_full_eight_provider_json_then_sse_with_redacted_report(self):
        gateway = FakeGateway()
        report = execute("http://127.0.0.1:8080", "", 8, gateway)
        self.assertEqual(report["phase"], "completed")
        self.assertEqual([x["provider"] for x in report["providers"]], list(PROVIDERS))
        self.assertEqual(gateway.posts, [
            call for p in PROVIDERS for call in [(p, False), (p, True)]
        ])
        for row in report["providers"]:
            self.assertEqual(row["json"]["status"], "passed")
            self.assertEqual(row["sse"]["status"], "passed")
            self.assertEqual(row["restart"]["status"], "skipped")
            self.assertEqual(row["repeat"]["status"], "skipped")
        serialized = json.dumps(report)
        self.assertNotIn("Reply exactly OK", serialized)
        self.assertNotIn("model-", serialized)
        self.assertNotIn("secret", serialized)
        self.assertNotIn("conversation_id", serialized)

    def test_failed_json_isolated_and_sse_skipped_for_that_provider(self):
        calls = []

        def sender(provider, _model, stream):
            calls.append((provider, stream))
            if provider == "grok" and not stream:
                raise urllib.error.HTTPError("secret", 502, "private error", None, None)
            return {"status": "passed", "request_id": str(uuid.uuid4())}

        rows = run_matrix({p: "model" for p in PROVIDERS}, sender)
        grok = next(row for row in rows if row["provider"] == "grok")
        self.assertEqual(grok["json"], {"status": "failed", "category": "http_502"})
        self.assertEqual(grok["sse"], {"status": "skipped", "reason": "json_not_passed"})
        self.assertNotIn(("grok", True), calls)
        self.assertEqual(len(calls), 15)

    def test_unadvertised_models_produce_eight_explicit_outcomes(self):
        rows = run_matrix({"chatgpt": "model"}, lambda *_: {"status": "passed"})
        self.assertEqual(len(rows), 8)
        self.assertEqual(sum(row["json"]["status"] == "skipped" for row in rows), 7)

    def test_without_execute_no_network_access_or_hidden_success(self):
        output = io.StringIO()
        with patch.object(sys, "argv", ["installed_provider_smoke.py"]), patch(
            "installed_provider_smoke.execute", side_effect=AssertionError("network used")
        ), redirect_stdout(output):
            self.assertEqual(main(), 0)
        report = json.loads(output.getvalue())
        self.assertEqual(report["phase"], "not_executed")
        self.assertTrue(all(row["json"]["status"] == "skipped"
                            for row in report["providers"]))


if __name__ == "__main__":
    unittest.main()
