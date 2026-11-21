"""Input-gate contracts; no provider accounts, network or runtime access."""
import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

from provider_verification_inputs import (
    InputError, STAGES, blocked_report, load_inputs, main, resolve_inputs,
)

ROOT = Path(__file__).resolve().parents[1]


def template():
    return json.loads((ROOT / "docs/verification/run-inputs.template.json").read_text())


def complete():
    record = template()
    record["selection"].update(providers_and_models={"metaai": "fixture", "chatgpt": "fixture"},
                               outputs=["json", "sse"], content_policy="plain_text")
    record["target"].update(deployment_type="docker", gateway_origin="http://127.0.0.1:1234",
                            build_choice="isolated_merged_main", expected_commit="a" * 40,
                            image_digest_or_native_binary_digest="sha256:" + "b" * 64,
                            attestation_plan="fixture", configuration_label="fixture",
                            resource_ownership_boundary="fixture")
    record["session"].update(source="saved_session", account_type="dedicated_test",
                             local_authorization_method_label="fixture")
    record["scenarios"] = {field: False for field in record["scenarios"]}
    record["scenarios"].update(normal_replies=True, memory_recall=True)
    record["permissions"]["live_requests"] = True
    record["limits"].update(maximum_requests_per_provider=4, minimum_interval_ms=5000,
                            maximum_concurrency=1, queue_timeout_seconds=30,
                            response_timeout_seconds={"metaai": 120, "chatgpt": 150},
                            maximum_duration_seconds=1800, daily_or_cost_cap="fixture_cap",
                            stop_conditions="stop_provider_on_failure",
                            retry_before_dispatch_policy="no_retry")
    record["acceptance"]["mode"] = "baseline"
    record["evidence"].update(local_output_directory="fixture", retention="fixture",
                              sharing="local_only")
    return record


class InputGateTests(unittest.TestCase):
    def test_explicit_provider_selection_does_not_choose_models_or_other_providers(self):
        record = template()
        record["selection"]["providers_and_models"] = {"metaai": None, "chatgpt": None}
        plan = resolve_inputs(record)
        self.assertEqual(plan["selected_providers"], ["metaai", "chatgpt"])
        self.assertFalse(plan["inputs_complete"])
        self.assertIn("selection.providers_and_models.metaai", plan["missing_inputs"])
        self.assertIn("selection.providers_and_models.chatgpt", plan["missing_inputs"])

    def test_complete_inputs_never_become_observed_passes(self):
        record = complete()
        plan = resolve_inputs(record)
        self.assertTrue(plan["inputs_complete"])
        self.assertEqual(plan["normal_and_recall_requests_per_provider"], 4)
        report = blocked_report(record, plan)
        self.assertIsNone(report["gateway_commit"])
        self.assertIsNone(report["runtime_digest"])
        self.assertEqual(len(report["results"]), 2 * len(STAGES))
        self.assertEqual({row["status"] for row in report["results"]}, {"blocked"})
        self.assertTrue(all(row["evidence_kind"] is None for row in report["results"]))

    def test_false_consent_and_unsafe_numeric_values_do_not_resolve(self):
        for field, value in [("maximum_concurrency", True),
                             ("maximum_requests_per_provider", 3),
                             ("minimum_interval_ms", -1),
                             ("response_timeout_seconds", {"metaai": 120})]:
            record = complete()
            record["limits"][field] = value
            self.assertIn("limits." + field, resolve_inputs(record)["missing_inputs"])
        record = complete()
        record["permissions"]["live_requests"] = False
        self.assertIn("permissions.live_requests", resolve_inputs(record)["missing_inputs"])

    def test_selected_side_effects_require_matching_inputs(self):
        record = complete()
        record["scenarios"].update(fresh_capture=True, graceful_restart=True, soak=True)
        missing = resolve_inputs(record)["missing_inputs"]
        for field in ["session.chrome_available", "session.source",
                      "permissions.restart_disposable_resources", "permissions.load_testing",
                      "acceptance.sample_count", "acceptance.soak_duration_seconds"]:
            self.assertIn(field, missing)

    def test_reports_do_not_echo_private_input_values_or_unknown_fields(self):
        record = complete()
        record["target"]["gateway_origin"] = "https://private.invalid/?token=secret"
        record["session"]["local_authorization_method_label"] = "private-account-secret"
        record["private"] = "cookie=secret"
        encoded = json.dumps(blocked_report(record, resolve_inputs(record)))
        self.assertNotIn("secret", encoded)
        self.assertNotIn("private", encoded)

    def test_blocked_report_matches_the_versioned_result_shape(self):
        schema = json.loads((ROOT / "docs/verification/result.schema.json").read_text())
        record = complete()
        report = blocked_report(record, resolve_inputs(record))
        self.assertEqual(set(report), set(schema["required"]))
        item = schema["properties"]["results"]["items"]
        for row in report["results"]:
            self.assertEqual(set(row), set(item["required"]))
            for field, definition in item["properties"].items():
                if "enum" in definition:
                    self.assertIn(row[field], definition["enum"])

    def test_duplicate_oversized_and_invalid_records_fail_without_values(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "input.json"
            for content, category in [('{"x":1,"x":"secret"}', "duplicate_input_field"),
                                      ('x' * 65_537, "input_record_too_large"),
                                      ('{"x":NaN}', "invalid_input_record"),
                                      ('["secret"]', "invalid_input_record")]:
                path.write_text(content)
                with self.assertRaises(InputError) as error:
                    load_inputs(path)
                self.assertEqual(str(error.exception), category)

    def test_cli_with_unset_inputs_exits_blocked_without_network(self):
        record = template()
        record["selection"]["providers_and_models"] = {"metaai": None, "chatgpt": None}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "inputs.json"
            report = Path(directory) / "result.json"
            path.write_text(json.dumps(record))
            with patch("urllib.request.urlopen", side_effect=AssertionError("network forbidden")):
                with redirect_stdout(io.StringIO()) as output:
                    self.assertEqual(main(["--inputs", str(path), "--report", str(report)]), 2)
            self.assertEqual(json.loads(output.getvalue())["live_requests_sent"], 0)
            self.assertTrue(report.exists())

    def test_versions_and_provider_names_cannot_expand_scope(self):
        for value in [True, 2, "1"]:
            record = complete()
            record["standard_version"] = value
            with self.assertRaisesRegex(InputError, "unsupported_input_version"):
                resolve_inputs(record)
        record = complete()
        record["selection"]["providers_and_models"]["unknown_secret"] = "private"
        with self.assertRaisesRegex(InputError, "invalid_provider_selection"):
            resolve_inputs(record)


if __name__ == "__main__":
    unittest.main()
