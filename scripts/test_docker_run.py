import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts" / "docker-run.sh"


class DockerHelperContractTests(unittest.TestCase):
    def make_tools(self, root: Path) -> tuple[Path, Path]:
        bin_dir = root / "bin"
        bin_dir.mkdir()
        calls = root / "docker-calls.jsonl"
        docker = bin_dir / "docker"
        docker.write_text(
            """#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["DOCKER_CALLS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps(args) + "\\n")
if args[:2] in (["network", "inspect"], ["image", "inspect"]):
    raise SystemExit(1)
if args[:2] == ["buildx", "version"] and os.environ.get("BUILDX_MISSING") == "1":
    raise SystemExit(1)
if args[:2] == ["buildx", "build"]:
    raise SystemExit(int(os.environ.get("BUILD_EXIT", "0")))
if args and args[0] == "run" and args[-1] == "/vault/pairing-admin.key":
    print("test-only-pairing-admin-secret-32chars")
if args[:2] == ["container", "inspect"]:
    raise SystemExit(0 if os.environ.get("CONTAINER_EXISTS") == "1" else 1)
""",
            encoding="utf-8",
        )
        docker.chmod(0o755)
        opener = bin_dir / "open"
        opener.write_text(
            """#!/usr/bin/env python3
import os, sys
with open(os.environ["OPENED_URLS"], "a", encoding="utf-8") as stream:
    stream.write(sys.argv[1] + "\\n")
""",
            encoding="utf-8",
        )
        opener.chmod(0o755)
        curl = bin_dir / "curl"
        curl.write_text(
            """#!/usr/bin/env python3
import os, sys
with open(os.environ["CURL_CALLS"], "a", encoding="utf-8") as stream:
    stream.write(sys.argv[-1] + "\\n")
sys.stdout.write(os.environ.get("CURL_RESPONSE", ""))
raise SystemExit(1 if os.environ.get("CURL_FAIL") == "1" else 0)
""",
            encoding="utf-8",
        )
        curl.chmod(0o755)
        return bin_dir, calls

    def build(self, overrides=None):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        bin_dir, calls = self.make_tools(root)
        env = {**os.environ, "PATH": f"{bin_dir}:{os.environ.get('PATH', '')}",
               "DOCKER_CALLS": str(calls), "CHAT_GATEWAY_IMAGE": "gateway:test"}
        env.update(overrides or {})
        result = subprocess.run(["bash", str(HELPER), "build"], cwd=ROOT, env=env,
                                text=True, capture_output=True, check=False)
        return result, [json.loads(line) for line in calls.read_text().splitlines()]

    def test_build_uses_buildx_and_loads_the_exact_image(self):
        result, calls = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        command = next(args for args in calls if args[:2] == ["buildx", "build"])
        self.assertEqual(command, ["buildx", "build", "--load", "--pull", "--tag", "gateway:test", str(ROOT)])
        self.assertFalse(any(args and args[0] == "build" for args in calls))

    def test_missing_buildx_fails_before_attempting_a_legacy_build(self):
        result, calls = self.build({"BUILDX_MISSING": "1"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Buildx is required", result.stderr)
        self.assertIn("brew install docker-buildx", result.stderr)
        self.assertFalse(any(args[:2] == ["buildx", "build"] or args[0] == "build" for args in calls))

    def test_build_failure_is_not_hidden_or_retried_with_a_stale_builder(self):
        result, calls = self.build({"BUILD_EXIT": "71"})
        self.assertEqual(result.returncode, 71)
        self.assertEqual(sum(args[:2] == ["buildx", "build"] for args in calls), 1)

    def test_build_forwards_platform_and_cache_as_single_arguments(self):
        options = {"CHAT_GATEWAY_BUILD_PLATFORM": "linux/arm64",
                   "CHAT_GATEWAY_BUILD_TARGET": "builder",
                   "CHAT_GATEWAY_BUILD_CACHE_FROM": "type=gha,scope=arm64-2gb",
                   "CHAT_GATEWAY_BUILD_CACHE_TO": "type=gha,scope=arm64-2gb,mode=max"}
        result, calls = self.build(options)
        self.assertEqual(result.returncode, 0, result.stderr)
        command = next(args for args in calls if args[:2] == ["buildx", "build"])
        for flag, value in [("--platform", options["CHAT_GATEWAY_BUILD_PLATFORM"]),
                            ("--target", options["CHAT_GATEWAY_BUILD_TARGET"]),
                            ("--cache-from", options["CHAT_GATEWAY_BUILD_CACHE_FROM"]),
                            ("--cache-to", options["CHAT_GATEWAY_BUILD_CACHE_TO"])]:
            self.assertEqual(command[command.index(flag) + 1], value)

    def pairing_code(self, response, fail=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir, _calls = self.make_tools(root)
            env = {**os.environ, "PATH": f"{bin_dir}:{os.environ.get('PATH', '')}",
                   "DOCKER_CALLS": str(root / "docker-calls.jsonl"),
                   "CURL_CALLS": str(root / "curl-calls.txt"),
                   "CONTAINER_EXISTS": "1", "CURL_RESPONSE": response,
                   "CURL_FAIL": "1" if fail else "0"}
            return subprocess.run(["bash", str(HELPER), "pairing-code"],
                                  cwd=ROOT, env=env, text=True, capture_output=True)

    def test_pairing_code_prints_only_eight_digits_and_preserves_leading_zeros(self):
        for response in ['{"code":"00123456"}', '{\n  "code" : "00123456"\n}']:
            with self.subTest(response=response):
                result = self.pairing_code(response)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "00123456\n")
                self.assertNotIn("test-only-pairing-admin-secret", result.stderr)

    def test_pairing_code_rejects_missing_nonstring_and_invalid_codes(self):
        for payload in [{}, {"code": 12345678}, {"code": "1234567"},
                        {"code": "123456789"}, {"code": "abcdefgh"}]:
            with self.subTest(payload=payload):
                result = self.pairing_code(json.dumps(payload))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertIn("valid pairing code", result.stderr)

    def test_pairing_code_never_prints_a_code_when_http_rotation_fails(self):
        result = self.pairing_code('{"code":"12345678"}', fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")

    def test_start_publishes_only_loopback_and_keeps_vault_key_in_readonly_volume(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir, calls = self.make_tools(root)
            config = root / "config.yaml"
            config.write_text("providers: []\\n", encoding="utf-8")
            config.chmod(0o600)
            env = os.environ.copy()
            env.update({
                "PATH": f"{bin_dir}:{env.get('PATH', '')}",
                "DOCKER_CALLS": str(calls),
                "CURL_CALLS": str(root / "curl-calls.txt"),
                "CHAT_GATEWAY_CONFIG": str(config),
                "CHAT_GATEWAY_EXTENSION_ID": "abcdefghijklmnopabcdefghijklmnop",
                "PORT": "18080",
            })

            result = subprocess.run(
                ["bash", str(HELPER), "start"],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            invocations = [json.loads(line) for line in calls.read_text(encoding="utf-8").splitlines()]
            gateway_run = next(args for args in invocations if args[:2] == ["run", "-d"])

            self.assertEqual(gateway_run[gateway_run.index("--memory") + 1], "4g")
            self.assertEqual(gateway_run[gateway_run.index("--memory-swap") + 1], "4g")
            self.assertIn("127.0.0.1:18080:18080", gateway_run)
            self.assertFalse(any("0.0.0.0" in arg for arg in gateway_run))
            self.assertEqual(config.stat().st_mode & 0o777, 0o600)
            self.assertIn("type=volume,source=chat-gateway-config,target=/run/config,readonly", gateway_run)
            self.assertIn("/run/config/config.yaml", gateway_run)
            self.assertFalse(any("type=bind,source=" + str(config) in arg for arg in gateway_run))
            self.assertIn("CHAT_GATEWAY_SESSION_STORE=vault", gateway_run)
            self.assertIn("CHAT_GATEWAY_VAULT_KEY_FILE=/run/secrets/vault.key", gateway_run)
            self.assertIn(
                "type=volume,source=chat-gateway-vault-key,target=/run/secrets,readonly",
                gateway_run,
            )
            self.assertIn(
                "type=volume,source=chat-gateway-sessions,target=/app/sessions",
                gateway_run,
            )
            self.assertIn("CHAT_GATEWAY_EXTENSION_ID=abcdefghijklmnopabcdefghijklmnop", gateway_run)
            self.assertFalse(any("CHAT_GATEWAY_VAULT_KEY=" in arg for arg in gateway_run))
            self.assertEqual(
                (root / "curl-calls.txt").read_text(encoding="utf-8").strip(),
                "http://127.0.0.1:18080/health",
            )
            self.assertIn("is healthy", result.stdout)


    def test_start_fails_closed_when_health_check_never_succeeds(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir, calls = self.make_tools(root)
            config = root / "config.yaml"
            config.write_text("providers: []\\n", encoding="utf-8")
            env = os.environ.copy()
            env.update({
                "PATH": f"{bin_dir}:{env.get('PATH', '')}",
                "DOCKER_CALLS": str(calls),
                "CURL_CALLS": str(root / "curl-calls.txt"),
                "CURL_FAIL": "1",
                "CHAT_GATEWAY_CONFIG": str(config),
                "CHAT_GATEWAY_STARTUP_TIMEOUT_SECONDS": "1",
            })
            result = subprocess.run(
                ["bash", str(HELPER), "start"],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("did not become healthy", result.stderr)
            self.assertIn("docker-run.sh logs", result.stderr)

    def test_status_returns_nonzero_when_gateway_health_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir, calls = self.make_tools(root)
            env = os.environ.copy()
            env.update({
                "PATH": f"{bin_dir}:{env.get('PATH', '')}",
                "DOCKER_CALLS": str(calls),
                "CURL_CALLS": str(root / "curl-calls.txt"),
                "CURL_FAIL": "1",
                "CONTAINER_EXISTS": "1",
            })
            result = subprocess.run(
                ["bash", str(HELPER), "status"],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Health: unhealthy", result.stderr)

    def test_login_opens_provider_site_in_external_browser_without_collecting_password(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir, _calls = self.make_tools(root)
            opened = root / "opened-urls.txt"
            env = os.environ.copy()
            env.update({
                "PATH": f"{bin_dir}:{env.get('PATH', '')}",
                "DOCKER_CALLS": str(root / "docker-calls.jsonl"),
                "OPENED_URLS": str(opened),
            })

            result = subprocess.run(
                ["bash", str(HELPER), "login", "chatgpt"],
                cwd=ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(opened.read_text(encoding="utf-8").strip(), "https://chatgpt.com/")
            self.assertIn("normal external browser", result.stdout)
            self.assertIn("passwords are never collected", result.stdout.lower())


if __name__ == "__main__":
    unittest.main()
