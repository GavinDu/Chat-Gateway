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
    PROVIDERS, FOCUS_PROVIDERS, SmokeFailure, _identity, _safe_error,
    _failed_http_attempt, execute, main,
    model_inventory, parse_selected_providers, run_matrix, validate_json, validate_sse,
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

    def test_request_id_requires_canonical_non_nil_uuid_for_json_and_sse(self):
        valid = str(uuid.uuid4())
        valid_sse = consume_sse([
            'data: {"choices":[{"delta":{"content":"OK"}}]}\n',
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n',
            "data: [DONE]\n",
        ])
        headers = sample_headers()
        headers["X-Request-Id"] = valid
        validate_json(answer(), headers, "chatgpt", "fixture")
        validate_sse(valid_sse, headers, "chatgpt", "fixture")

        for bad in (
            "00000000-0000-0000-0000-000000000000",
            valid.upper(),
            "{" + valid + "}",
            "urn:uuid:" + valid,
            " " + valid,
            valid + " ",
            "token=secret session=private",
            "",
        ):
            headers["X-Request-Id"] = bad
            for validate, response in (
                (validate_json, answer()),
                (validate_sse, valid_sse),
            ):
                with self.subTest(bad=bad, validator=validate.__name__):
                    with self.assertRaises(SmokeFailure) as error:
                        validate(response, headers, "chatgpt", "fixture")
                    self.assertEqual(error.exception.category, "missing_request_id")
                    self.assertNotIn("secret", str(error.exception))

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
            "source_sha": "a" * 40, "build_timestamp_utc": "2026-11-17T23:42:05Z",
            "credential": "secret"}}),
            {"health": "ok", "source_sha": "a" * 40,
             "build_timestamp_utc": "2026-11-17T23:42:05Z"})
        self.assertEqual(_identity({"status": "error", "source_sha": "b" * 40,
                                    "commit_sha": "c" * 40}),
                         {"health": "unverified", "source_sha": "unknown",
                          "build_timestamp_utc": "unknown"})
        self.assertEqual(_safe_error(urllib.error.HTTPError(
            "https://secret.invalid/path?token=private", 502,
            "private detail", None, None)), "http_502")
        self.assertEqual(_safe_error(RuntimeError("secret upstream body")), "invalid_response")


class InstalledProvenanceTests(unittest.TestCase):
    def test_provenance_is_always_explicit_and_never_inferred_from_checkout(self):
        for health in [
            {"status": "ok"},
            {"status": "ok", "build": None},
            {"status": "ok", "build": "a" * 40},
            {"status": "ok", "source_sha": "b" * 40, "commit_sha": "c" * 40},
            {"status": "ok", "build": {"source_sha": "unknown"}},
        ]:
            self.assertEqual(_identity(health), {
                "health": "ok",
                "source_sha": "unknown",
                "build_timestamp_utc": "unknown",
            })

    def test_build_metadata_rejects_untrusted_freeform_strings_and_bad_dates(self):
        bad_shas = [
            "A" * 40, "a" * 39, "a" * 41, "a" * 40 + "\\nsecret",
            "https://private.example?token=secret", "unknown", 123, None,
        ]
        bad_times = [
            "2026-02-30T20:15:30Z", "2026-11-17T24:15:30Z",
            "2026-11-17T20:15:30+00:00", "2026-11-17T20:15:30.000Z",
            "2026-11-17T20:15:30Z\\r\\nsecret", "private.example",
            None, 17,
        ]
        for value in bad_shas:
            self.assertEqual(_identity({"status": "ok",
                                        "build": {"source_sha": value}})["source_sha"],
                             "unknown")
        for value in bad_times:
            self.assertEqual(_identity({"status": "ok", "build": {
                "build_timestamp_utc": value}})["build_timestamp_utc"], "unknown")

    def test_build_metadata_is_independently_validated_per_field(self):
        self.assertEqual(_identity({"status": "ok", "build": {
            "source_sha": "a" * 40,
            "build_timestamp_utc": "2026-02-30T00:00:00Z",
        }}), {
            "health": "ok",
            "source_sha": "a" * 40,
            "build_timestamp_utc": "unknown",
        })
        self.assertEqual(_identity({"status": "ok", "build": {
            "source_sha": "not-a-sha",
            "build_timestamp_utc": "2026-11-17T23:42:05Z",
            "secret": "cookie=very-secret",
        }}), {
            "health": "ok",
            "source_sha": "unknown",
            "build_timestamp_utc": "2026-11-17T23:42:05Z",
        })


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
        self.turns = []

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
        prompt = content["messages"][0]["content"]
        req_id = req.get_header("X-conversation-id")
        assert isinstance(req_id, str)
        self.turns.append((provider, content["stream"], req_id, prompt))
        headers = sample_headers(provider, req_id)
        if content["stream"]:
            data = (b'data: {"choices":[{"delta":{"content":"OK"}}]}\n'
                    b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n'
                    b'data: [DONE]\n')
        else:
            data = json.dumps(answer("TWO" if prompt == "Reply exactly TWO"
                                     else "OK")).encode()
        return FakeResponse(data, headers)


class FaultyRepeatGateway(FakeGateway):
    def __init__(self, fault):
        super().__init__()
        self.fault = fault
        self.previous_json_request = None

    def open(self, req, timeout):
        response = super().open(req, timeout)
        if not req.full_url.endswith("/v1/chat/completions"):
            return response
        payload = json.loads(req.data)
        prompt = payload["messages"][0]["content"]
        if prompt == "Reply exactly OK" and not payload["stream"]:
            self.previous_json_request = response.headers["X-Request-Id"]
        if prompt != "Reply exactly TWO":
            return response
        if self.fault == "stale":
            response.body = json.dumps(answer("OK")).encode()
        elif self.fault == "duplicate_request":
            response.headers["X-Request-Id"] = self.previous_json_request
        elif self.fault == "wrong_provider":
            response.headers["X-Gateway-Provider"] = "metaai"
        elif self.fault == "wrong_conversation":
            response.headers["X-Conversation-Id"] = str(uuid.uuid4())
        elif self.fault == "incomplete":
            response.body = json.dumps(answer("TWO", finish="length")).encode()
        return response


class ExecutionContractTests(unittest.TestCase):
    def test_full_eight_provider_json_then_sse_with_redacted_report(self):
        gateway = FakeGateway()
        report = execute("http://127.0.0.1:8080", "", 8, gateway,
                         selected_providers=PROVIDERS)
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

    def test_error_response_correlates_only_canonical_gateway_uuid(self):
        request_id = str(uuid.uuid4())

        class RefuseBodyRead:
            def read(self, *_args):
                raise AssertionError("HTTP error body must not be read")

        def failure(header):
            return urllib.error.HTTPError(
                "https://private.invalid/?token=secret", 502,
                "Authorization: bearer hidden", Headers({
                    "X-Request-Id": header,
                    "X-Private-Cookie": "secret-cookie-value",
                }), RefuseBodyRead(),
            )

        result = _failed_http_attempt(failure(request_id))
        self.assertEqual(result, {
            "status": "failed", "category": "http_502",
            "request_id": request_id,
        })
        for secret in ("secret", "Authorization", "hidden",
                       "private.invalid", "cookie", "bearer"):
            self.assertNotIn(secret, json.dumps(result))

        for bad in (
            request_id.upper(),
            "00000000-0000-0000-0000-000000000000",
            "{"+request_id+"}", "token=secret", request_id+" hidden",
            "x"*40000, None, 42,
        ):
            with self.subTest(invalid_header=str(bad)[:50]):
                safe = _failed_http_attempt(failure(bad))
                self.assertEqual(safe, {"status": "failed", "category": "http_502"})
                self.assertNotIn("secret", json.dumps(safe))

        self.assertEqual(_failed_http_attempt(
            urllib.error.URLError("token=secret upstream address")
        ), {"status": "failed", "category": "gateway_unreachable"})
        self.assertEqual(_failed_http_attempt(
            TimeoutError("private account id")
        ), {"status": "failed", "category": "gateway_unreachable"})

    def test_http_error_ids_are_scoped_to_the_failed_json_sse_or_repeat_attempt(self):
        json_id, sse_id, repeat_id = [str(uuid.uuid4()) for _ in range(3)]

        def fail(code, request_id):
            return urllib.error.HTTPError(
                "https://private.invalid/path?token=secret", code,
                "secret error", Headers({"X-Request-Id": request_id}), None
            )

        calls = []
        def json_fails(provider, _model, stream):
            calls.append((provider, stream))
            raise fail(502, json_id)

        rows = run_matrix({"chatgpt": "opaque-model"}, json_fails,
                          selected_providers=("chatgpt",))
        focused = rows[0]
        self.assertEqual(focused["json"], {
            "status": "failed", "category": "http_502",
            "request_id": json_id,
        })
        self.assertEqual(focused["sse"],
                         {"status": "skipped", "reason": "json_not_passed"})
        self.assertEqual(calls, [("chatgpt", False)])
        self.assertTrue(all(row["json"]["reason"] == "owner_deferred"
                            for row in rows[1:]))

        def sse_fails(_provider, _model, stream):
            if stream:
                raise fail(504, sse_id)
            return {"status": "passed", "request_id": str(uuid.uuid4())}

        rows = run_matrix({"chatgpt": "opaque-model"}, sse_fails,
                          selected_providers=("chatgpt",),
                          repeat_sender=lambda *_: (_ for _ in ()).throw(
                              AssertionError("repeat must be skipped when SSE fails")))
        self.assertEqual(rows[0]["sse"], {
            "status": "failed", "category": "http_504",
            "request_id": sse_id,
        })
        self.assertEqual(rows[0]["repeat"],
                         {"status": "skipped", "reason": "prerequisite_not_passed"})

        rows = run_matrix({"chatgpt": "opaque-model"},
                          lambda *_: {"status": "passed", "request_id": str(uuid.uuid4())},
                          selected_providers=("chatgpt",),
                          repeat_sender=lambda *_: (_ for _ in ()).throw(
                              fail(503, repeat_id)))
        self.assertEqual(rows[0]["repeat"], {
            "status": "failed", "category": "http_503",
            "request_id": repeat_id,
        })
        self.assertEqual(rows[0]["sse"]["status"], "passed")
        self.assertNotIn("secret", json.dumps(rows))

    def test_http_200_validation_failure_preserves_only_current_response_uuid(self):
        class Faulty200Gateway(FakeGateway):
            def __init__(self, mode):
                super().__init__()
                self.mode = mode
                self.failed_id = None

            def open(self, req, timeout):
                response = super().open(req, timeout)
                if not req.full_url.endswith("/v1/chat/completions"):
                    return response
                stream = json.loads(req.data)["stream"]
                if self.mode in ("missing_done", "error_after_content") and not stream:
                    return response
                if self.mode not in ("missing_done", "error_after_content") and stream:
                    return response
                self.failed_id = response.headers["X-Request-Id"]
                if self.mode == "wrong_answer":
                    response.body = json.dumps(answer("stale")).encode()
                elif self.mode == "bad_json":
                    response.body = b'{"credentials": "secret",'
                elif self.mode == "read_error":
                    def private_read(_maximum):
                        raise OSError("private body URL https://secret.invalid")
                    response.read = private_read
                elif self.mode == "missing_id":
                    response.headers["X-Request-Id"] = "cookie=secret"
                    response.body = json.dumps(answer("stale")).encode()
                elif self.mode == "missing_done":
                    response.body = (
                        b'data: {"choices":[{"delta":{"content":"OK"}}]}\n'
                        b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n')
                elif self.mode == "error_after_content":
                    response.body = (
                        b'data: {"choices":[{"delta":{"content":"OK"}}]}\n'
                        b'data: {"error":{"message":"secret upstream body"}}\n'
                        b'data: [DONE]\n')
                return response

        for mode, category in (
            ("wrong_answer", "unexpected_answer"),
            ("bad_json", "invalid_json"),
            ("read_error", "gateway_unreachable"),
            ("missing_id", "missing_request_id"),
            ("missing_done", "sse_incomplete"),
            ("error_after_content", "sse_incomplete"),
        ):
            with self.subTest(mode=mode):
                gateway = Faulty200Gateway(mode)
                report = execute("http://127.0.0.1:8080", "", 8, gateway,
                                 selected_providers=("chatgpt",))
                self.assertEqual(report["phase"], "completed")
                focused = report["providers"][0]
                failed = focused["sse"] if mode in ("missing_done", "error_after_content") else focused["json"]
                self.assertEqual(failed["status"], "failed")
                self.assertEqual(failed["category"], category)
                if mode == "missing_id":
                    self.assertNotIn("request_id", failed)
                else:
                    self.assertEqual(failed["request_id"], gateway.failed_id)
                self.assertEqual(report["providers"][1]["json"],
                                 {"status":"skipped","reason":"owner_deferred"})
                for private in ("secret", "stale", "upstream", "credentials",
                                "private body", "conversation_id", "model-chatgpt"):
                    self.assertNotIn(private, json.dumps(report))
                if mode in ("missing_done", "error_after_content"):
                    self.assertEqual(focused["json"]["status"], "passed")
                else:
                    self.assertEqual(focused["sse"],
                                     {"status":"skipped","reason":"json_not_passed"})

    def test_failed_json_isolated_and_sse_skipped_for_that_provider(self):
        calls = []

        def sender(provider, _model, stream):
            calls.append((provider, stream))
            if provider == "grok" and not stream:
                raise urllib.error.HTTPError("secret", 502, "private error", None, None)
            return {"status": "passed", "request_id": str(uuid.uuid4())}

        rows = run_matrix({p: "model" for p in PROVIDERS}, sender,
                          selected_providers=PROVIDERS)
        grok = next(row for row in rows if row["provider"] == "grok")
        self.assertEqual(grok["json"], {"status": "failed", "category": "http_502"})
        self.assertEqual(grok["sse"], {"status": "skipped", "reason": "json_not_passed"})
        self.assertNotIn(("grok", True), calls)
        self.assertEqual(len(calls), 15)

    def test_unadvertised_models_produce_eight_explicit_outcomes(self):
        rows = run_matrix({"chatgpt": "model"}, lambda *_: {"status": "passed"},
                          selected_providers=PROVIDERS)
        self.assertEqual(len(rows), 8)
        self.assertEqual(sum(row["json"]["status"] == "skipped" for row in rows), 7)

    def test_default_only_tests_focus_two_even_when_eight_are_advertised(self):
        gateway = FakeGateway()
        report = execute("http://127.0.0.1:8080", "", 8, gateway)
        self.assertEqual(gateway.posts, [
            ("chatgpt", False), ("chatgpt", True),
            ("metaai", False), ("metaai", True),
        ])
        self.assertEqual(FOCUS_PROVIDERS, ("chatgpt", "metaai"))
        self.assertEqual(len(report["providers"]), 8)
        for row in report["providers"]:
            if row["provider"] in FOCUS_PROVIDERS:
                self.assertEqual(row["json"]["status"], "passed")
                self.assertEqual(row["sse"]["status"], "passed")
            else:
                self.assertEqual(row["json"], {"status":"skipped","reason":"owner_deferred"})
                self.assertEqual(row["sse"], {"status":"skipped","reason":"owner_deferred"})
        self.assertNotIn("model-", json.dumps(report))

    def test_opt_in_second_turn_reuses_one_conversation_and_requires_distinct_answer(self):
        gateway = FakeGateway()
        report = execute("http://127.0.0.1:8080", "", 8, gateway, repeat=True)
        self.assertEqual(report["phase"], "completed")
        self.assertEqual(len(report["providers"]), 8)
        for provider in FOCUS_PROVIDERS:
            records = [item for item in gateway.turns if item[0] == provider]
            self.assertEqual(len(records), 4)
            # Existing fresh JSON and SSE are independent conversations.
            self.assertNotEqual(records[0][2], records[1][2])
            # Explicit repeat has one new shared conversation, but the second
            # user turn and answer cannot be the previous turn's stale reply.
            self.assertEqual(records[2][2], records[3][2])
            self.assertNotEqual(records[1][2], records[2][2])
            self.assertEqual(records[2][3], "Reply exactly OK")
            self.assertEqual(records[3][3], "Reply exactly TWO")
            self.assertEqual(records[2][1:], (False, records[2][2], "Reply exactly OK"))
            self.assertEqual(records[3][1:], (False, records[3][2], "Reply exactly TWO"))
            row = next(item for item in report["providers"]
                       if item["provider"] == provider)
            self.assertEqual(row["json"]["status"], "passed")
            self.assertEqual(row["sse"]["status"], "passed")
            self.assertEqual(row["repeat"]["status"], "passed")
            self.assertNotEqual(row["repeat"]["first_request_id"],
                                row["repeat"]["second_request_id"])
            self.assertEqual(row["restart"],
                             {"status": "skipped", "reason": "separate_acceptance"})
        for row in report["providers"]:
            if row["provider"] not in FOCUS_PROVIDERS:
                self.assertEqual(row["repeat"],
                                 {"status": "skipped", "reason": "owner_deferred"})
        redacted = json.dumps(report)
        for private in ("Reply exactly", "TWO", "model-chatgpt", "conversation_id"):
            self.assertNotIn(private, redacted)

    def test_distinct_repeat_rejects_stale_request_reuse_wrong_scope_and_incomplete_finish(self):
        for fault, expected in [
            ("stale", "unexpected_answer"),
            ("duplicate_request", "reused_request_id"),
            ("wrong_provider", "wrong_provider"),
            ("wrong_conversation", "wrong_conversation"),
            ("incomplete", "incomplete_finish"),
        ]:
            with self.subTest(fault=fault):
                gateway = FaultyRepeatGateway(fault)
                report = execute("http://127.0.0.1:8080", "", 8, gateway,
                                 selected_providers=("chatgpt",), repeat=True)
                row = next(x for x in report["providers"]
                           if x["provider"] == "chatgpt")
                self.assertEqual(row["json"]["status"], "passed")
                self.assertEqual(row["sse"]["status"], "passed")
                self.assertEqual(row["repeat"]["status"], "failed")
                self.assertEqual(row["repeat"]["category"], expected)
                if fault == "duplicate_request":
                    # The cross-turn duplicate check runs after both HTTP
                    # responses are complete; it has no new failed request
                    # to claim as the origin of that composite violation.
                    self.assertNotIn("request_id", row["repeat"])
                else:
                    observed = row["repeat"]["request_id"]
                    self.assertEqual(str(uuid.UUID(observed)), observed)
                    self.assertNotEqual(uuid.UUID(observed).int, 0)
                self.assertEqual(row["restart"]["status"], "skipped")
                self.assertNotIn("Reply exactly", json.dumps(report))
                self.assertNotIn("model-chatgpt", json.dumps(report))

    def test_repeat_opt_in_never_runs_if_json_or_sse_prerequisite_fails(self):
        repeat_calls = []
        def sender(provider, _model, stream):
            if provider == "chatgpt" and stream:
                raise SmokeFailure("sse_incomplete")
            return {"status": "passed", "request_id": str(uuid.uuid4())}
        rows = run_matrix({"chatgpt": "model", "metaai": "model"}, sender,
                          repeat_sender=lambda provider, model: repeat_calls.append(
                              (provider, model)) or {"status": "passed"})
        chatgpt = next(row for row in rows if row["provider"] == "chatgpt")
        self.assertEqual(chatgpt["sse"]["status"], "failed")
        self.assertEqual(chatgpt["repeat"],
                         {"status": "skipped", "reason": "prerequisite_not_passed"})
        self.assertEqual(repeat_calls, [("metaai", "model")])

    def test_expected_binary_source_sha_preflights_without_sending_on_mismatch(self):
        class VersionedGateway(FakeGateway):
            def __init__(self, source_sha):
                super().__init__()
                self.source_sha = source_sha
                self.calls = []

            def open(self, req, timeout):
                self.calls.append(req.full_url.rsplit("/", 1)[-1])
                if req.full_url.endswith("/health"):
                    return FakeResponse(json.dumps({
                        "status": "ok",
                        "build": {"source_sha": self.source_sha},
                    }).encode())
                return super().open(req, timeout)

        wrong = VersionedGateway("a" * 40)
        report = execute("http://127.0.0.1:8080", "", 8, wrong,
                         expected_source_sha="b" * 40)
        self.assertEqual(report["phase"], "preflight_failed")
        self.assertEqual(report["identity"]["source_sha"], "a" * 40)
        self.assertEqual(len(report["providers"]), 8)
        self.assertTrue(all(row["json"] == {
            "status": "skipped", "reason": "source_sha_mismatch",
        } for row in report["providers"]))
        self.assertEqual(wrong.calls, ["health"])
        self.assertEqual(wrong.posts, [])

        unknown = VersionedGateway(None)
        report = execute("http://127.0.0.1:8080", "", 8, unknown,
                         expected_source_sha="a" * 40)
        self.assertEqual(report["phase"], "preflight_failed")
        self.assertEqual(report["identity"]["source_sha"], "unknown")
        self.assertEqual(unknown.calls, ["health"])
        self.assertEqual(unknown.posts, [])

        for bad in ("A" * 40, "a" * 39, "a" * 40 + " token=secret",
                    "unknown", ""):
            gateway = VersionedGateway("a" * 40)
            with self.subTest(bad=bad), self.assertRaises(SmokeFailure) as error:
                execute("http://127.0.0.1:8080", "", 8, gateway,
                        expected_source_sha=bad)
            self.assertEqual(error.exception.category, "invalid_expected_source_sha")
            self.assertEqual(gateway.calls, [])
            self.assertNotIn("secret", str(error.exception))

        correct = VersionedGateway("a" * 40)
        report = execute("http://127.0.0.1:8080", "", 8, correct,
                         expected_source_sha="a" * 40)
        self.assertEqual(report["phase"], "completed")
        self.assertEqual(correct.posts, [
            ("chatgpt", False), ("chatgpt", True),
            ("metaai", False), ("metaai", True),
        ])
        self.assertNotIn("token=secret", json.dumps(report))

    def test_explicit_selection_rejects_unknown_duplicates_and_empty(self):
        self.assertEqual(parse_selected_providers("chatgpt,metaai"), FOCUS_PROVIDERS)
        self.assertEqual(parse_selected_providers("metaai"), ("metaai",))
        for bad in ("", " ", "all", "chatgpt,chatgpt", "chatgpt,", "metaai,unknown"):
            with self.subTest(bad=bad), self.assertRaises(SmokeFailure) as error:
                parse_selected_providers(bad)
            self.assertEqual(error.exception.category, "invalid_selection")

    def test_invalid_selection_fails_before_first_network_request(self):
        gateway = FakeGateway()
        with self.assertRaises(SmokeFailure) as error:
            execute("http://127.0.0.1:8080", "", 8, gateway,
                    selected_providers=("chatgpt","invalid"))
        self.assertEqual(error.exception.category, "invalid_selection")
        self.assertEqual(gateway.posts, [])

    def test_repeat_flag_without_execute_never_contacts_installed_gateway(self):
        output = io.StringIO()
        with patch.object(sys, "argv", [
            "installed_provider_smoke.py", "--repeat",
        ]), patch("installed_provider_smoke.execute",
                  side_effect=AssertionError("unexpected gateway network")), redirect_stdout(output):
            self.assertEqual(main(), 0)
        report = json.loads(output.getvalue())
        self.assertEqual(report["phase"], "not_executed")
        self.assertTrue(all(row["repeat"]["status"] == "skipped"
                            for row in report["providers"]))

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
