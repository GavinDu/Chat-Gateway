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
if args[:2] in (["network", "inspect"], ["image", "inspect"], ["container", "inspect"]):
    raise SystemExit(1)
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
raise SystemExit(1 if os.environ.get("CURL_FAIL") == "1" else 0)
""",
            encoding="utf-8",
        )
        curl.chmod(0o755)
        return bin_dir, calls

    def test_start_publishes_only_loopback_and_keeps_vault_key_in_readonly_volume(self):
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

            self.assertIn("127.0.0.1:18080:18080", gateway_run)
            self.assertFalse(any("0.0.0.0" in arg for arg in gateway_run))
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
            self.assertGreaterEqual(
                len((root / "curl-calls.txt").read_text(encoding="utf-8").splitlines()),
                1,
            )

    def test_login_opens_provider_site_in_external_browser_without_collecting_password(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir, _calls = self.make_tools(root)
            opened = root / "opened-urls.txt"
            env = os.environ.copy()
            env.update({
                "PATH": f"{bin_dir}:{env.get('PATH', '')}",
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
