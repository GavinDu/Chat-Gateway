import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PARSER = ROOT / "scripts" / "manus_runtime_status.py"
HELPER = ROOT / "scripts" / "docker-run.sh"
spec = importlib.util.spec_from_file_location("manus_runtime_status", PARSER)
runtime_status = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime_status)


def response(enabled=True, linked=True, auth="unknown"):
    return json.dumps({
        "ok": True,
        "providers": [
            {"provider": "chatgpt", "enabled": True, "linked": False},
            {"provider": "manus", "enabled": enabled, "linked": linked,
             "authentication": auth},
        ],
        "vault_mode": True,
    }).encode("utf-8")


class ManusRuntimeStatusContractTests(unittest.TestCase):
    def test_reports_only_allowlisted_running_gateway_state(self):
        result = runtime_status.fixed_manus_status(
            response(enabled=True, linked=True, auth="verified"))
        self.assertEqual(result[0], "Manus running configuration: enabled")
        self.assertEqual(result[1], "Manus saved gateway session: present")
        self.assertIn("verified by gateway", result[2])
        self.assertEqual(result[3], "Manus Chat/Agent live acceptance: unverified")
        self.assertEqual(runtime_status.fixed_manus_status(
            response(enabled=False, linked=True, auth="verified"))[2],
            "Manus gateway-reported authentication: unverified")
        self.assertEqual(runtime_status.fixed_manus_status(
            response(enabled=True, linked=False, auth="verified"))[2],
            "Manus gateway-reported authentication: unverified")
        self.assertEqual(runtime_status.fixed_manus_status(
            response(enabled=True, linked=True, auth="needs_login"))[2],
            "Manus gateway-reported authentication: login needed")

    def test_fails_closed_on_untyped_duplicated_missing_or_oversized_status(self):
        bad = [
            b"", b"not-json", b"[]", b'{"ok": false, "providers":[]}',
            b'{"ok":true,"providers":{}}',
            b'{"ok":true,"providers":[{"provider":"chatgpt"}]}',
            b'{"ok":true,"providers":[{"provider":"manus","enabled":"true","linked":true}]}',
            b'{"ok":true,"providers":[{"provider":"manus","enabled":true,"linked":1}]}',
            b'{"ok":true,"providers":[{"provider":"manus","enabled":true,"linked":true},'
            b'{"provider":"manus","enabled":false,"linked":false}]}',
            b'{"ok":true,"providers":[{"provider":"manus","enabled":true,'
            b'"enabled":false,"linked":true}]}',
            b'{"ok":true,"providers":[]}PRIVATE-ACCOUNT-SECRET',
            b'{"ok":true,"providers":[' + b' ' * runtime_status.MAX_BYTES + b']}',
        ]
        for raw in bad:
            with self.subTest(raw_prefix=raw[:45]):
                with self.assertRaises((ValueError, UnicodeDecodeError, TypeError)):
                    runtime_status.fixed_manus_status(raw)

    def test_unknown_or_secret_bearing_auth_is_never_echoed(self):
        raw = json.dumps({
            "ok": True, "providers": [
                {"provider": "manus", "enabled": True, "linked": True,
                 "authentication": "token=private-secret"}
            ], "opaque_account": "private-secret",
        }).encode("utf-8")
        output = "\n".join(runtime_status.fixed_manus_status(raw))
        self.assertIn("unverified", output)
        self.assertNotIn("private-secret", output)
        self.assertNotIn("token=", output)

    def run_command(self, *, fixture, port="8080", fail=False):
        with tempfile.TemporaryDirectory() as home:
            home = Path(home)
            curl = home / "curl"
            record = home / "curl-args.json"
            curl.write_text(
                "#!/usr/bin/env python3\n"
                "import json,os,sys\n"
                "with open(os.environ['FAKE_CURL_ARGS'],'w') as f:\n"
                "    json.dump(sys.argv[1:],f)\n"
                "sys.stdout.write(os.environ.get('FAKE_RESPONSE',''))\n"
                "raise SystemExit(int(os.environ.get('FAKE_EXIT','0')))\n"
            )
            curl.chmod(0o700)
            env = {**os.environ,
                   "PATH": f"{home}:{os.environ.get('PATH', '')}",
                   "PORT": port,
                   "FAKE_CURL_ARGS": str(record),
                   "FAKE_RESPONSE": fixture,
                   "FAKE_EXIT": "1" if fail else "0"}
            proc = subprocess.run(
                ["bash", str(HELPER), "manus-runtime-status"],
                cwd=ROOT, env=env, capture_output=True, text=True,
                check=False, timeout=20)
            args = json.loads(record.read_text()) if record.exists() else None
            return proc, args

    def test_operator_command_is_loopback_only_no_tokens_and_redacted(self):
        proc, args = self.run_command(
            fixture=response(enabled=True, linked=True, auth="verified").decode())
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("Manus running configuration: enabled", proc.stdout)
        self.assertIn("Chat/Agent live acceptance: unverified", proc.stdout)
        self.assertEqual(args[-1], "http://127.0.0.1:8080/relay/v1/status")
        self.assertIn("--noproxy", args)
        self.assertEqual(args[args.index("--noproxy") + 1], "*")
        self.assertIn("--max-time", args)
        self.assertIn("--max-filesize", args)
        self.assertEqual(args[args.index("--max-filesize") + 1], "131072")
        self.assertIn("--fail", args)
        self.assertNotIn("--location", args)
        self.assertNotIn("Authorization", json.dumps(args))
        self.assertNotIn("private", proc.stdout + proc.stderr)

    def test_bad_port_transport_and_payload_never_echo_gateway_content(self):
        for port in ["bad;command", "65536", "0", "-1", "127.0.0.1:8080"]:
            proc, args = self.run_command(fixture="private-secret", port=port)
            self.assertNotEqual(proc.returncode, 0)
            self.assertIsNone(args, "invalid port must not invoke curl")
            self.assertEqual(proc.stderr.strip(), "Manus runtime status: unavailable")
            self.assertEqual(proc.stdout, "")
        for fixture, failed in [
            ("private-secret", False),
            ('{"ok":true,"providers":[],"token":"private-secret"}', False),
            ("private-secret", True),
        ]:
            proc, args = self.run_command(fixture=fixture, fail=failed)
            self.assertNotEqual(proc.returncode, 0)
            self.assertIsNotNone(args)
            self.assertEqual(proc.stderr.strip(), "Manus runtime status: unavailable")
            self.assertNotIn("private-secret", proc.stdout + proc.stderr)


if __name__ == "__main__":
    unittest.main()
